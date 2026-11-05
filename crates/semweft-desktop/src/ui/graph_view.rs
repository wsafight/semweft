use super::*;
use crate::graph::{Diagram, NODE_HEIGHT, NODE_WIDTH};
use gpui::{MouseButton, PathBuilder, Pixels, Point, canvas, point};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

#[derive(PartialEq, Eq)]
struct LayoutKey {
    view: String,
    side: String,
    root: String,
    file: String,
    relations: usize,
    nodes: usize,
    dependencies: bool,
    comparison: bool,
    grouped: bool,
    collapsed: BTreeSet<String>,
}

pub(super) struct GraphView {
    zoom: f32,
    pan: (f32, f32),
    drag: Option<Point<Pixels>>,
    grouped: bool,
    collapsed: BTreeSet<String>,
    selected: String,
    dependencies: bool,
    comparison: bool,
    mode: usize,
    positions: BTreeMap<String, (f32, f32)>,
    node_drag: Option<(String, Point<Pixels>, (f32, f32))>,
    viewport: Rc<Cell<(f32, f32)>>,
    minimap_origin: Rc<Cell<(f32, f32)>>,
    search_index: usize,
    layout: RefCell<Option<(LayoutKey, Rc<Diagram>)>>,
    placed: RefCell<Option<(usize, usize, u64, Rc<Diagram>)>>,
    positions_revision: u64,
    comparison_pairs: Option<(
        String,
        BTreeSet<(String, String)>,
        BTreeSet<(String, String)>,
    )>,
}

impl Default for GraphView {
    fn default() -> Self {
        Self {
            zoom: 1.,
            pan: (0., 0.),
            drag: None,
            grouped: false,
            collapsed: BTreeSet::new(),
            selected: String::new(),
            dependencies: false,
            comparison: false,
            mode: 0,
            positions: BTreeMap::new(),
            node_drag: None,
            viewport: Rc::new(Cell::new((800., 500.))),
            minimap_origin: Rc::new(Cell::new((0., 0.))),
            search_index: 0,
            layout: RefCell::default(),
            placed: RefCell::default(),
            positions_revision: 0,
            comparison_pairs: None,
        }
    }
}

impl LayoutKey {
    /// Compare against the live state without cloning strings or the collapsed set.
    fn matches(&self, desktop: &Desktop) -> bool {
        self.view == desktop.data.review_key
            && self.side == desktop.navigation_side
            && self.root == desktop.navigation_symbol
            && self.file == desktop.source_file
            && self.relations == desktop.navigation.result.relations.len()
            && self.nodes == desktop.navigation.result.nodes.len()
            && self.dependencies == desktop.graph_view.dependencies
            && self.comparison == desktop.graph_view.comparison
            && self.grouped == desktop.graph_view.grouped
            && self.collapsed == desktop.graph_view.collapsed
    }
}

fn comparison_pairs(raw: &Value) -> (BTreeSet<(String, String)>, BTreeSet<(String, String)>) {
    let pairs = |field: &str| {
        raw["impact"]["relation_changes"][field]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|relation| {
                Some((
                    relation["from"].as_str()?.to_owned(),
                    relation["to"].as_str()?.to_owned(),
                ))
            })
            .collect::<BTreeSet<_>>()
    };
    (pairs("added"), pairs("removed"))
}

impl Desktop {
    fn graph_key(&self) -> String {
        format!(
            "{}/{}/{}/{}/{}/{}/{}/{}",
            self.data.review_key,
            self.navigation_side,
            self.navigation_symbol,
            self.graph_view.dependencies,
            self.source_file,
            self.references,
            self.direction,
            self.graph_view.comparison
        )
    }

    pub(super) fn restore_graph(&mut self) {
        if let Some(layout) = self.session.graph_layouts.get(&self.graph_key()) {
            self.graph_view.zoom = if layout.zoom.is_finite() {
                layout.zoom.clamp(0.1, 2.)
            } else {
                1.
            };
            self.graph_view.pan = if layout.pan.0.is_finite() && layout.pan.1.is_finite() {
                layout.pan
            } else {
                (0., 0.)
            };
            self.graph_view.positions = layout
                .positions
                .iter()
                .filter(|(_, position)| position.0.is_finite() && position.1.is_finite())
                .take(160)
                .map(|(id, position)| (id.clone(), *position))
                .collect();
            self.graph_view.collapsed = layout.collapsed.clone();
            self.graph_view.grouped = layout.grouped;
            self.graph_view.mode = layout.mode.min(2);
            self.graph_view.positions_revision += 1;
        }
    }

    fn save_graph(&mut self, cx: &mut Context<Self>) {
        let key = self.graph_key();
        if !self.session.graph_layouts.contains_key(&key)
            && self.session.graph_layouts.len() >= 32
            && let Some(oldest) = self.session.graph_layouts.keys().next().cloned()
        {
            self.session.graph_layouts.remove(&oldest);
        }
        self.session.graph_layouts.insert(
            key,
            crate::model::GraphLayout {
                zoom: self.graph_view.zoom,
                pan: self.graph_view.pan,
                positions: self.graph_view.positions.clone(),
                collapsed: self.graph_view.collapsed.clone(),
                grouped: self.graph_view.grouped,
                mode: self.graph_view.mode,
            },
        );
        self.message = match self.session.save(&self.launch.data) {
            Ok(()) => "当前图布局已保存".into(),
            Err(error) => format!("布局保存失败：{error}"),
        };
        cx.notify();
    }

    fn placed_diagram(&self) -> Rc<Diagram> {
        let base = self.diagram();
        let mode = self.graph_view.mode;
        let revision = self.graph_view.positions_revision;
        let pointer = Rc::as_ptr(&base) as usize;
        if let Some((cached_pointer, cached_mode, cached_revision, placed)) =
            self.graph_view.placed.borrow().as_ref()
            && *cached_pointer == pointer
            && *cached_mode == mode
            && *cached_revision == revision
        {
            return placed.clone();
        }
        let placed = if mode == 0 && self.graph_view.positions.is_empty() {
            base
        } else {
            let mut diagram = (*base).clone();
            diagram.arrange(mode, &self.graph_view.positions);
            Rc::new(diagram)
        };
        *self.graph_view.placed.borrow_mut() = Some((pointer, mode, revision, placed.clone()));
        placed
    }

    fn change_graph(&mut self, cx: &mut Context<Self>) {
        let added: Vec<crate::model::Relation> =
            serde_json::from_value(self.data.raw["impact"]["relation_changes"]["added"].clone())
                .unwrap_or_default();
        let removed: Vec<crate::model::Relation> =
            serde_json::from_value(self.data.raw["impact"]["relation_changes"]["removed"].clone())
                .unwrap_or_default();
        let pairs = |relations: &[crate::model::Relation]| {
            relations
                .iter()
                .map(|relation| (relation.from.clone(), relation.to.clone()))
                .collect::<BTreeSet<_>>()
        };
        let comparison_pairs = (pairs(&added), pairs(&removed));
        let mut relations = self.data.impact.relations.clone();
        relations.extend(added);
        relations.extend(removed);
        if relations.is_empty() {
            self.message = "当前变更没有捕获关系证据".into();
            cx.notify();
            return;
        }
        let root = if relations.iter().any(|relation| {
            relation.from == self.navigation_symbol || relation.to == self.navigation_symbol
        }) {
            self.navigation_symbol.clone()
        } else {
            relations[0].from.clone()
        };
        let referenced = relations
            .iter()
            .flat_map(|relation| [&relation.from, &relation.to])
            .cloned()
            .collect::<BTreeSet<_>>();
        self.navigation.reset();
        self.navigation.result.relations = relations;
        self.navigation.result.nodes = self
            .data
            .symbols
            .symbols
            .iter()
            .chain(&self.data.base_symbols.symbols)
            .filter(|symbol| referenced.contains(symbol.symbol_id.as_str()))
            .cloned()
            .collect();
        self.navigation_symbol = root;
        self.graph_view = GraphView {
            comparison: true,
            comparison_pairs: Some((
                self.data.review_key.clone(),
                comparison_pairs.0,
                comparison_pairs.1,
            )),
            ..Default::default()
        };
        self.restore_graph();
        cx.notify();
    }

    fn find_graph_node(&mut self, cx: &mut Context<Self>) {
        let diagram = self.placed_diagram();
        let matching = diagram
            .nodes
            .iter()
            .filter(|node| self.matches(&node.label) || self.matches(&node.location.file))
            .collect::<Vec<_>>();
        if let Some(node) = matching.get(self.graph_view.search_index % matching.len().max(1)) {
            self.graph_view.selected = node.id.clone();
            let (width, height) = self.graph_view.viewport.get();
            self.graph_view.pan = (
                width / 2. - (node.x + NODE_WIDTH / 2.) * self.graph_view.zoom,
                height / 2. - (node.y + NODE_HEIGHT / 2.) * self.graph_view.zoom,
            );
            self.graph_view.search_index += 1;
        } else {
            self.message = "当前可见图没有匹配节点；可继续加载或重设中心".into();
        }
        cx.notify();
    }

    fn graph_minimap(&self, diagram: &Diagram, cx: &Context<Self>) -> gpui::AnyElement {
        let (width, height) = diagram.size();
        let scale = (164. / width).min(94. / height);
        let origin = self.graph_view.minimap_origin.clone();
        let marker = canvas(
            |_, _, _| (),
            move |bounds, _, _, _| {
                origin.set((f32::from(bounds.origin.x), f32::from(bounds.origin.y)));
            },
        )
        .absolute()
        .size_full();
        let viewport = self.graph_view.viewport.get();
        let zoom = self.graph_view.zoom;
        rsx! { <div id="graph-minimap" class="absolute right-3 bottom-3 w-[180px] h-[110px] bg-white border rounded-lg overflow-hidden" border_color={rgb(LINE)}
            onMouseDown={(MouseButton::Left, cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                cx.stop_propagation();
                let origin = this.graph_view.minimap_origin.get();
                let world = ((f32::from(event.position.x) - origin.0 - 8.) / scale, (f32::from(event.position.y) - origin.1 - 8.) / scale);
                this.graph_view.pan = (viewport.0 / 2. - world.0 * this.graph_view.zoom, viewport.1 / 2. - world.1 * this.graph_view.zoom);
                cx.notify();
            }))}>
            {marker}
            {for node in &diagram.nodes { <div key={format!("mini-{}", node.id)} class="absolute rounded-sm" left={px(8. + node.x * scale)} top={px(8. + node.y * scale)} w={px((NODE_WIDTH * scale).max(2.))} h={px((NODE_HEIGHT * scale).max(2.))} bg={rgb(if node.id == self.graph_view.selected { GREEN } else { 0xb6ccc1 })} /> }}
            <div class="absolute border" border_color={rgb(GREEN)} left={px(8. - self.graph_view.pan.0 / zoom * scale)} top={px(8. - self.graph_view.pan.1 / zoom * scale)} w={px(viewport.0 / zoom * scale)} h={px(viewport.1 / zoom * scale)} />
        </div> }.into_any_element()
    }
    fn diagram(&self) -> Rc<Diagram> {
        if let Some((previous, diagram)) = self.graph_view.layout.borrow().as_ref()
            && previous.matches(self)
        {
            return diagram.clone();
        }
        let key = LayoutKey {
            view: self.data.review_key.clone(),
            side: self.navigation_side.clone(),
            root: self.navigation_symbol.clone(),
            file: self.source_file.clone(),
            relations: self.navigation.result.relations.len(),
            nodes: self.navigation.result.nodes.len(),
            dependencies: self.graph_view.dependencies,
            comparison: self.graph_view.comparison,
            grouped: self.graph_view.grouped,
            collapsed: self.graph_view.collapsed.clone(),
        };
        let diagram = if self.graph_view.dependencies {
            let edges = self.data.dependency_edges();
            let symbols = edges
                .keys()
                .map(|file| Symbol {
                    symbol_id: file.clone(),
                    qualified_name: file.clone(),
                    location: crate::model::Location {
                        file: file.clone(),
                        line: 1,
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .collect::<Vec<_>>();
            let relations = edges
                .iter()
                .flat_map(|(from, targets)| {
                    targets.iter().map(move |to| crate::model::Relation {
                        from: from.clone(),
                        to: to.clone(),
                        certainty: "candidate".into(),
                        ..Default::default()
                    })
                })
                .collect::<Vec<_>>();
            let root = if edges.contains_key(&self.source_file) {
                self.source_file.as_str()
            } else {
                edges.keys().next().map(String::as_str).unwrap_or("")
            };
            Diagram::build(
                root,
                symbols.iter(),
                &relations,
                false,
                &self.graph_view.collapsed,
            )
        } else if self.navigation_symbol.is_empty() {
            Diagram::default()
        } else {
            Diagram::build(
                &self.navigation_symbol,
                self.data
                    .symbols_on(&self.navigation_side)
                    .symbols
                    .iter()
                    .chain(&self.navigation.result.nodes),
                &self.navigation.result.relations,
                self.graph_view.grouped,
                &self.graph_view.collapsed,
            )
        };
        let mut diagram = diagram;
        if self.graph_view.comparison && !self.graph_view.grouped {
            let fallback;
            let (added, removed) = match &self.graph_view.comparison_pairs {
                Some((key, added, removed)) if key == &self.data.review_key => (added, removed),
                _ => {
                    fallback = comparison_pairs(&self.data.raw);
                    (&fallback.0, &fallback.1)
                }
            };
            for edge in &mut diagram.edges {
                let pair = (
                    diagram.nodes[edge.from].id.clone(),
                    diagram.nodes[edge.to].id.clone(),
                );
                edge.change = match (added.contains(&pair), removed.contains(&pair)) {
                    (true, false) => 1,
                    (false, true) => -1,
                    (true, true) => 2,
                    _ => 0,
                };
            }
        }
        let diagram = Rc::new(diagram);
        *self.graph_view.layout.borrow_mut() = Some((key, diagram.clone()));
        diagram
    }

    fn export_graph(&self, cx: &mut Context<Self>) {
        let svg = self.placed_diagram().svg();
        let prompt = cx.prompt_for_new_path(&self.data.workspace, Some("semweft-graph.svg"));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = prompt.await {
                let result = fs::write(&path, svg);
                let _ = this.update(cx, |this, cx| {
                    this.message = match result {
                        Ok(()) => format!("已导出当前可见图：{}", path.display()),
                        Err(error) => format!("导出失败：{error}"),
                    };
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(super) fn show_dependency_graph(&mut self, cx: &mut Context<Self>) {
        self.graph_view = GraphView {
            dependencies: true,
            ..Default::default()
        };
        self.page = Page::Graph;
        self.restore_graph();
        cx.notify();
    }

    pub(super) fn reset_graph(&mut self) {
        self.graph_view = GraphView::default();
    }

    pub(super) fn check_graph_variant(&mut self, variant: usize) {
        match variant {
            0 => {
                self.graph_view.zoom = 0.7;
                self.graph_view.pan = (30., 20.);
            }
            1 => self.graph_view.grouped = true,
            2 => {
                let diagram = self.diagram();
                if let Some(node) = diagram.nodes.first() {
                    self.graph_view.selected = node.id.clone();
                    self.graph_view.collapsed.insert(node.id.clone());
                }
            }
            3 => self.graph_view.dependencies = true,
            4 => self.graph_view.mode = 1,
            5 => {
                self.graph_view.mode = 2;
                if let Some(node) = self.diagram().nodes.first() {
                    self.graph_view
                        .positions
                        .insert(node.id.clone(), (430., 280.));
                    self.graph_view.positions_revision += 1;
                    let placed = self.placed_diagram();
                    assert_eq!(placed.nodes[0].x, 430.);
                }
            }
            _ => {
                self.graph_view.comparison = true;
                let (added, removed) = comparison_pairs(&self.data.raw);
                self.graph_view.comparison_pairs =
                    Some((self.data.review_key.clone(), added, removed));
            }
        }
    }

    pub(super) fn graph_canvas(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let diagram = self.placed_diagram();
        let zoom = self.graph_view.zoom;
        let (pan_x, pan_y) = self.graph_view.pan;
        let selected = &self.graph_view.selected;
        let selected_node = diagram
            .nodes
            .iter()
            .find(|node| &node.id == selected)
            .cloned();
        let lines = diagram
            .edges
            .iter()
            .map(|edge| {
                let from = &diagram.nodes[edge.from];
                let to = &diagram.nodes[edge.to];
                let backwards = to.x < from.x;
                let from_x = if backwards {
                    from.x
                } else {
                    from.x + NODE_WIDTH
                };
                let to_x = if backwards { to.x + NODE_WIDTH } else { to.x };
                let highlighted = &from.id == selected || &to.id == selected;
                (
                    (
                        from_x * zoom + pan_x,
                        (from.y + NODE_HEIGHT / 2.) * zoom + pan_y,
                    ),
                    (
                        to_x * zoom + pan_x,
                        (to.y + NODE_HEIGHT / 2.) * zoom + pan_y,
                    ),
                    edge.candidate || edge.stale,
                    highlighted,
                    edge.from == edge.to,
                    edge.change,
                )
            })
            .collect::<Vec<_>>();
        let viewport = self.graph_view.viewport.clone();
        let edges = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                viewport.set((f32::from(bounds.size.width), f32::from(bounds.size.height)));
                for (from, to, uncertain, highlighted, recursive, change) in lines {
                    if from.0.max(to.0) < -100.
                        || from.0.min(to.0) > f32::from(bounds.size.width) + 100.
                        || from.1.max(to.1) < -100.
                        || from.1.min(to.1) > f32::from(bounds.size.height) + 100.
                    {
                        continue;
                    }
                    let start = bounds.origin + point(px(from.0), px(from.1));
                    let end = bounds.origin + point(px(to.0), px(to.1));
                    let mut path = PathBuilder::stroke(px(if highlighted { 2.5 } else { 1.3 }));
                    if uncertain {
                        path = path.dash_array(&[px(5.), px(4.)]);
                    }
                    path.move_to(start);
                    if recursive {
                        path.cubic_bezier_to(
                            end,
                            start + point(px(45.), px(-85.)),
                            end + point(px(-45.), px(-85.)),
                        );
                    } else {
                        let middle = (start.x + end.x) / 2.;
                        path.cubic_bezier_to(end, point(middle, start.y), point(middle, end.y));
                    }
                    let sign = if end.x >= start.x { 1. } else { -1. };
                    path.move_to(end + point(px(-sign * 7.), px(-4.)));
                    path.line_to(end);
                    path.line_to(end + point(px(-sign * 7.), px(4.)));
                    if let Ok(path) = path.build() {
                        window.paint_path(
                            path,
                            rgb(if change == 1 {
                                GREEN
                            } else if change == -1 {
                                0xc45b50
                            } else if highlighted {
                                GREEN
                            } else if uncertain {
                                AMBER
                            } else {
                                0x91b0a1
                            }),
                        );
                    }
                }
            },
        )
        .absolute()
        .size_full();
        rsx! {
            <div class="flex flex-col min-h-0 flex-1">
                <div class="flex items-center gap-2 p-2 bg-white border rounded-t-xl flex-wrap" border_color={rgb(LINE)}>
                    <ActionButton id="graph-zoom-out" compact onClick={cx.listener(|this, _, _, cx| { this.graph_view.zoom = (this.graph_view.zoom / 1.2).max(0.35); cx.notify(); })}>{"−"}</ActionButton>
                    {muted(format!("{:.0}%", zoom * 100.))}
                    <ActionButton id="graph-zoom-in" compact onClick={cx.listener(|this, _, _, cx| { this.graph_view.zoom = (this.graph_view.zoom * 1.2).min(2.); cx.notify(); })}>{"＋"}</ActionButton>
                    <ActionButton id="graph-reset" compact onClick={cx.listener(|this, _, _, cx| { this.graph_view.zoom = 1.; this.graph_view.pan = (0., 0.); this.graph_view.collapsed.clear(); this.graph_view.positions.clear(); this.graph_view.positions_revision += 1; this.save_graph(cx); })}>{"重置"}</ActionButton>
                    {...(!self.graph_view.dependencies).then(|| rsx! { <ActionButton id="graph-group" compact activeIf={self.graph_view.grouped} onClick={cx.listener(|this, _, _, cx| { this.graph_view.grouped = !this.graph_view.grouped; this.graph_view.collapsed.clear(); cx.notify(); })}>{"按文件聚合"}</ActionButton> })}
                    <ActionButton id="graph-svg" compact onClick={cx.listener(|this, _, _, cx| this.export_graph(cx))}>{"导出 SVG"}</ActionButton>
                    <ActionButton id="graph-changes" compact activeIf={self.graph_view.comparison} disabledIf={self.navigation.busy} onClick={cx.listener(|this, _, _, cx| this.change_graph(cx))}>{"双侧关系变化"}</ActionButton>
                    {for (mode, label) in ["横向", "纵向", "网格"].into_iter().enumerate() { <ActionButton id={("graph-layout", mode)} compact activeIf={self.graph_view.mode == mode} onClick={cx.listener(move |this, _, _, cx| { this.graph_view.mode = mode; this.graph_view.positions.clear(); this.graph_view.positions_revision += 1; this.save_graph(cx); })}>{label}</ActionButton> }}
                    <ActionButton id="graph-save-layout" compact onClick={cx.listener(|this, _, _, cx| this.save_graph(cx))}>{"保存布局"}</ActionButton>
                    <ActionButton id="graph-search" compact disabledIf={self.search_text.trim().is_empty()} onClick={cx.listener(|this, _, _, cx| this.find_graph_node(cx))}>{"定位搜索匹配"}</ActionButton>
                    <ActionButton id="graph-fit" compact onClick={cx.listener(|this, _, _, cx| { let diagram = this.placed_diagram(); let (width, height) = diagram.size(); let viewport = this.graph_view.viewport.get(); this.graph_view.zoom = ((viewport.0 - 32.) / width).min((viewport.1 - 32.) / height).clamp(0.1, 2.); this.graph_view.pan = (16., 16.); cx.notify(); })}>{"适应窗口"}</ActionButton>
                    {muted(format!("{} 节点 · {} 边 · {} 隐藏", diagram.nodes.len(), diagram.edges.len(), diagram.hidden))}
                    {muted(if self.graph_view.comparison { "双侧证据 · 绿边新增/红边删除 · 聚合后不标记边变化" } else { "拖动节点调整位置 · 空白处平移 · 鸟瞰图点击定位 · 虚线：候选/过期" })}
                </div>
                <div id="interactive-graph" class="relative flex-1 min-h-[240px] overflow-hidden bg-[#f6f8fb] border-x" border_color={rgb(LINE)}
                    onMouseDown={(MouseButton::Left, cx.listener(|this, event: &gpui::MouseDownEvent, _, _| { this.graph_view.drag = Some(event.position); }))}
                    onMouseUp={(MouseButton::Left, cx.listener(|this, _, _, cx| { this.graph_view.drag = None; if this.graph_view.node_drag.take().is_some() { this.save_graph(cx); } }))}
                    onMouseMove={cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                        if let Some((id, start, origin)) = &this.graph_view.node_drag {
                            if event.dragging() { this.graph_view.positions.insert(id.clone(), ((origin.0 + f32::from(event.position.x - start.x) / this.graph_view.zoom).clamp(0., 1_000_000.), (origin.1 + f32::from(event.position.y - start.y) / this.graph_view.zoom).clamp(0., 1_000_000.))); this.graph_view.positions_revision += 1; cx.notify(); }
                            else { this.graph_view.node_drag = None; }
                            return;
                        }
                        if event.dragging() { if let Some(previous) = this.graph_view.drag { this.graph_view.pan.0 += f32::from(event.position.x - previous.x); this.graph_view.pan.1 += f32::from(event.position.y - previous.y); cx.notify(); } this.graph_view.drag = Some(event.position); } else { this.graph_view.drag = None; }
                    })}
                    onScrollWheel={cx.listener(|this, event: &gpui::ScrollWheelEvent, _, cx| {
                        let delta = event.delta.pixel_delta(px(24.));
                        if event.modifiers.platform { this.graph_view.zoom = (this.graph_view.zoom * (f32::from(delta.y) * 0.008).exp()).clamp(0.35, 2.); }
                        else { this.graph_view.pan.0 += f32::from(delta.x); this.graph_view.pan.1 += f32::from(delta.y); }
                        cx.stop_propagation(); cx.notify();
                    })}>
                    {edges}
                    {for node in &diagram.nodes {
                        <div key={node.id.clone()} class="absolute rounded-lg border bg-white overflow-hidden cursor-pointer" left={px(node.x * zoom + pan_x)} top={px(node.y * zoom + pan_y)} w={px(NODE_WIDTH * zoom)} h={px(NODE_HEIGHT * zoom)} p={px(8. * zoom)}
                            border_color={rgb(if &node.id == selected { GREEN } else { 0xc9dcd1 })} hoverClass="bg-[#edf5f0]"
                            onMouseDown={(MouseButton::Left, cx.listener({ let id = node.id.clone(); let origin = (node.x, node.y); move |this, event: &gpui::MouseDownEvent, _, cx| { this.graph_view.selected = id.clone(); this.graph_view.node_drag = Some((id.clone(), event.position, origin)); cx.stop_propagation(); cx.notify(); } }))}
                            onClick={cx.listener({ let id = node.id.clone(); move |this, _, _, cx| { this.graph_view.selected = id.clone(); cx.stop_propagation(); cx.notify(); } })}>
                            <div class="truncate font-semibold" text_size={px(11. * zoom)}>{node.label.clone()}</div>
                            <div class="truncate mt-1" text_color={rgb(MUTED)} text_size={px(9. * zoom)}>{format!("{}:{}", node.location.file, node.location.line)}</div>
                            <div class="mt-1" text_color={rgb(GREEN)} text_size={px(9. * zoom)}>{if self.graph_view.collapsed.contains(&node.id) { "⊞ 已折叠" } else { "选择节点查看操作" }}</div>
                        </div>
                    }}
                    {self.graph_minimap(&diagram, cx)}
                </div>
                <div class="p-2 border rounded-b-xl flex gap-2 items-center bg-white" border_color={rgb(LINE)}>
                    {...selected_node.map(|node| rsx! {
                        <div class="flex gap-2 items-center min-w-0">
                            <div base={muted(node.label.clone())} class="truncate max-w-[300px]" />
                            <ActionButton id="graph-collapse" compact onClick={cx.listener({ let id = node.id.clone(); move |this, _, _, cx| { if !this.graph_view.collapsed.remove(&id) { this.graph_view.collapsed.insert(id.clone()); } cx.notify(); } })}>{"展开/折叠"}</ActionButton>
                            <ActionButton id="graph-open-source" compact disabledIf={node.location.file.is_empty()} onClick={cx.listener({ let location = node.location.clone(); move |this, _, _, cx| { let side = if this.graph_view.dependencies { "head".into() } else if this.graph_view.comparison && this.data.verify_location_on(&location, "base").is_ok() { "base".into() } else { this.navigation_side.clone() }; if this.graph_view.dependencies || this.data.verify_location_on(&location, &side).is_ok() { this.open_side(location.file.clone(), location.line.max(1), &side, true, cx); this.page = Page::Files; } else { this.message = "节点内容与捕获源码不一致".into(); cx.notify(); } } })}>{"源码"}</ActionButton>
                            {...(!self.graph_view.grouped && !self.graph_view.dependencies).then(|| rsx! { <ActionButton id="graph-recenter" compact onClick={cx.listener(move |this, _, _, cx| this.load_side_navigation(node.id.clone(), this.references, this.navigation_side.clone(), cx))}>{"以此为中心展开"}</ActionButton> })}
                        </div>
                    })}
                    {...selected.is_empty().then(|| muted("选择节点高亮相邻路径，并展开、折叠或打开源码"))}
                </div>
            </div>
        }.into_any_element()
    }
}
