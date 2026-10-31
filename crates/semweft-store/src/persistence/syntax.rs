use super::*;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SyntaxOrder {
    symbols: Vec<usize>,
    calls: Vec<usize>,
    occurrences: Vec<usize>,
    diagnostics: Vec<usize>,
}

/// Content-addressed per-file facts, with a compact ordering vector to preserve
/// arbitrary frontend ordering without putting global offsets into each shard.
pub(super) fn split(facts: &SyntaxFacts) -> (BTreeMap<String, SyntaxFacts>, SyntaxOrder) {
    let mut shards: BTreeMap<String, SyntaxFacts> = BTreeMap::new();
    for symbol in &facts.symbols {
        shards
            .entry(symbol.file.clone())
            .or_default()
            .symbols
            .push(symbol.clone());
    }
    for call in &facts.calls {
        shards
            .entry(call.file.clone())
            .or_default()
            .calls
            .push(call.clone());
    }
    for occurrence in &facts.occurrences {
        shards
            .entry(occurrence.file.clone())
            .or_default()
            .occurrences
            .push(occurrence.clone());
    }
    for diagnostic in &facts.diagnostics {
        shards
            .entry(diagnostic.file.clone().unwrap_or_default())
            .or_default()
            .diagnostics
            .push(diagnostic.clone());
    }
    let owners: BTreeMap<_, _> = facts
        .symbols
        .iter()
        .map(|s| (&s.symbol_id, &s.file))
        .collect();
    for (id, declaration) in &facts.declarations {
        let file = owners
            .get(id)
            .map(|file| (*file).clone())
            .unwrap_or_default();
        shards
            .entry(file)
            .or_default()
            .declarations
            .insert(id.clone(), declaration.clone());
    }
    for (file, skeleton) in &facts.skeletons {
        shards
            .entry(file.clone())
            .or_default()
            .skeletons
            .insert(file.clone(), skeleton.clone());
    }
    for block in &facts.impls {
        shards
            .entry(block.file.clone())
            .or_default()
            .impls
            .push(block.clone());
    }
    for (file, imports) in &facts.imports {
        shards
            .entry(file.clone())
            .or_default()
            .imports
            .insert(file.clone(), imports.clone());
    }
    let indices: BTreeMap<_, _> = shards
        .keys()
        .enumerate()
        .map(|(i, file)| (file.as_str(), i))
        .collect();
    let order = SyntaxOrder {
        symbols: facts
            .symbols
            .iter()
            .map(|s| indices[s.file.as_str()])
            .collect(),
        calls: facts
            .calls
            .iter()
            .map(|s| indices[s.file.as_str()])
            .collect(),
        occurrences: facts
            .occurrences
            .iter()
            .map(|s| indices[s.file.as_str()])
            .collect(),
        diagnostics: facts
            .diagnostics
            .iter()
            .map(|s| indices[s.file.as_deref().unwrap_or("")])
            .collect(),
    };
    (shards, order)
}

fn ordered<T>(groups: Vec<Vec<T>>, order: Vec<usize>) -> Result<Vec<T>> {
    let mut groups: Vec<_> = groups.into_iter().map(Vec::into_iter).collect();
    let mut result = Vec::with_capacity(order.len());
    for index in order {
        result.push(
            groups
                .get_mut(index)
                .and_then(Iterator::next)
                .context("index_store_error: invalid syntax ordering")?,
        );
    }
    ensure!(
        groups.iter_mut().all(|group| group.next().is_none()),
        "index_store_error: incomplete syntax ordering"
    );
    Ok(result)
}

pub(super) fn join(shards: Vec<SyntaxFacts>, order: SyntaxOrder) -> Result<SyntaxFacts> {
    let mut facts = SyntaxFacts::default();
    let (mut symbols, mut calls, mut occurrences, mut diagnostics) =
        (vec![], vec![], vec![], vec![]);
    for shard in shards {
        symbols.push(shard.symbols);
        calls.push(shard.calls);
        occurrences.push(shard.occurrences);
        diagnostics.push(shard.diagnostics);
        facts.declarations.extend(shard.declarations);
        facts.skeletons.extend(shard.skeletons);
        facts.imports.extend(shard.imports);
        facts.impls.extend(shard.impls);
    }
    facts.impls.sort_by(|a, b| {
        (&a.file, a.span.start, &a.impl_id).cmp(&(&b.file, b.span.start, &b.impl_id))
    });
    facts.symbols = ordered(symbols, order.symbols)?;
    facts.calls = ordered(calls, order.calls)?;
    facts.occurrences = ordered(occurrences, order.occurrences)?;
    facts.diagnostics = ordered(diagnostics, order.diagnostics)?;
    Ok(facts)
}
