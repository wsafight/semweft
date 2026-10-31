use semweft_core::{Manifest, SyntaxFacts};
pub struct Frontends {
    rust: semweft_lang_rust::RustParser,
    #[cfg(feature = "go-experimental")]
    go: semweft_lang_go::GoParser,
    #[cfg(feature = "web-experimental")]
    web: semweft_lang_web::WebParser,
}
pub fn version() -> String {
    #[allow(unused_mut, clippy::useless_vec)]
    let mut versions = vec![semweft_lang_rust::PARSER_VERSION];
    #[cfg(feature = "go-experimental")]
    versions.push(semweft_lang_go::PARSER_VERSION);
    #[cfg(feature = "web-experimental")]
    versions.push(semweft_lang_web::PARSER_VERSION);
    versions.join(";")
}
impl Frontends {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            rust: semweft_lang_rust::RustParser::new()?,
            #[cfg(feature = "go-experimental")]
            go: semweft_lang_go::GoParser::new()?,
            #[cfg(feature = "web-experimental")]
            web: semweft_lang_web::WebParser::new()?,
        })
    }
    pub fn analyze(&mut self, manifest: &Manifest) -> Result<SyntaxFacts, String> {
        #[allow(unused_mut)]
        let mut facts = self.rust.analyze(manifest)?;
        #[cfg(feature = "go-experimental")]
        {
            let go = self.go.analyze(manifest)?;
            facts.symbols.extend(go.symbols);
            facts.calls.extend(go.calls);
            facts.occurrences.extend(go.occurrences);
            facts.diagnostics.extend(go.diagnostics);
            facts.skeletons.extend(go.skeletons);
            facts.declarations.extend(go.declarations);
            facts.imports.extend(go.imports);
            facts.symbols.sort_by(|a, b| a.symbol_id.cmp(&b.symbol_id));
            facts
                .calls
                .sort_by(|a, b| a.callsite_id.cmp(&b.callsite_id));
            facts.diagnostics.sort();
            facts.diagnostics.dedup();
        }
        #[cfg(feature = "web-experimental")]
        {
            let web = self.web.analyze(manifest)?;
            facts.symbols.extend(web.symbols);
            facts.calls.extend(web.calls);
            facts.occurrences.extend(web.occurrences);
            facts.diagnostics.extend(web.diagnostics);
            facts.skeletons.extend(web.skeletons);
            facts.declarations.extend(web.declarations);
            facts.imports.extend(web.imports);
            facts.symbols.sort_by(|a, b| a.symbol_id.cmp(&b.symbol_id));
            facts
                .calls
                .sort_by(|a, b| a.callsite_id.cmp(&b.callsite_id));
            facts.diagnostics.sort();
            facts.diagnostics.dedup();
        }
        Ok(facts)
    }
}
