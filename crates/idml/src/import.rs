//! IDML package → Document.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::sync::Arc;

use designcraft_color::cms::lab;
use designcraft_color::{Color, ColorType, Gradient, GradientKind, GradientStop, Swatch, SwatchValue, swatch};
use designcraft_doc::{
    Asset, AssetId, BaselineGrid, CellRange, CharAttrs, CharFormat, CharRun, CharacterStyle, Columns, ColumnsKind, Content, DirectionalFeather,
    DocSettings, Document, DropShadow, Effects, Fill, GradientFeather, GridAlign, GridRelative, Guide, Item, ItemId, Kerning, LAYER_COLORS, Layer,
    LayerId, ListType, Margins, ObjectStyle, Orientation, Page, PageId, PageSide, ParaAttrs, ParaFormat, ParagraphStyle, ParentInfo, RowHeightMode,
    Rule, Section, Shape, SpanColumns, Spread, SpreadId, Story, StoryId, Stroke, Styles, TabStop, Table, TextFrame, TextFrameOptions, TextWrap,
    story as st,
};
use designcraft_geom::corners::{Corner, CornerOptions};
use designcraft_geom::{Affine, Anchor, PathData, Point, Rect, SubPath};

use crate::names::{self, CHAR_BUILTINS, OBJECT_BUILTINS, PARA_BUILTINS, unescape_id};
use crate::xml::{El, Node, parse};
use crate::{IdmlError, MIMETYPE, Result, base64_decode, sniff_image};

mod document;
mod frame;
mod item;
mod object_style;
mod package;
mod spread;
mod story;
mod styles;
mod swatches;
mod table;
mod text_attrs;
mod values;

use document::IndexTopic;
use frame::margins_of;
use package::Package;
#[cfg(test)]
pub(crate) use package::{percent_decode, uri_to_path};
use text_attrs::mojikumi_row;
use values::{nums, xmp_date};

/// Import an IDML package without reading its linked images from disk: this crate never touches
/// the file system, so a caller that wants the links brings its own reader
/// ([`import_idml_with`]). The engine passes one that checks the automation roots.
pub fn import_idml(bytes: &[u8]) -> Result<Document> {
    import_idml_with(bytes, &|_| None)
}

/// Import with a custom reader for linked files (`path` → bytes).
pub fn import_idml_with(bytes: &[u8], read_link: &dyn Fn(&str) -> Option<Vec<u8>>) -> Result<Document> {
    import_idml_report(bytes, read_link).map(|r| r.document)
}

/// An imported IDML document and what the import could not bring in.
#[derive(Debug)]
pub struct Imported {
    pub document: Document,
    /// One line per problem that did not stop the import (a part listed in `designmap.xml` but
    /// absent from the package, …).
    pub warnings: Vec<String>,
}

/// Import with a custom reader for linked files, reporting the problems that did not stop it.
pub fn import_idml_report(bytes: &[u8], read_link: &dyn Fn(&str) -> Option<Vec<u8>>) -> Result<Imported> {
    import_idml_report_with_previews(bytes, read_link, &HashMap::new())
}

/// [`import_idml_report`], with stand-in images for graphics whose file neither the package nor
/// `read_link` provides: `previews` maps a graphic's `Self` to an image (an InDesign document's
/// screen preview of a linked file). The graphic keeps its link, so relinking replaces it.
pub fn import_idml_report_with_previews(
    bytes: &[u8],
    read_link: &dyn Fn(&str) -> Option<Vec<u8>>,
    previews: &HashMap<String, Arc<Vec<u8>>>,
) -> Result<Imported> {
    let mut pkg = Package::open(bytes)?;
    if let Some(m) = pkg.part("mimetype")?
        && String::from_utf8_lossy(&m).trim() != MIMETYPE
    {
        return Err(IdmlError::NotIdml(format!("unexpected mimetype `{}`", String::from_utf8_lossy(&m).trim())));
    }
    let dm = pkg.part("designmap.xml")?.ok_or_else(|| IdmlError::NotIdml("missing designmap.xml".into()))?;
    let root = parse(&dm).map_err(|msg| IdmlError::Part { part: "designmap.xml".into(), msg })?;
    if root.local() != "Document" {
        return Err(IdmlError::NotIdml(format!("designmap root is <{}>", root.name)));
    }
    // Flatten includes: every `idPkg:*` child is replaced by the children of its part's root.
    let mut top: Vec<El> = Vec::new();
    let mut warnings = Vec::new();
    for c in root.elements() {
        if c.name.starts_with("idPkg:") {
            let Some(src) = c.get("src") else { continue };
            let Some(data) = pkg.part(src)? else {
                warnings.push(format!("{src}: listed in designmap.xml but missing from the package; its contents were not imported"));
                continue;
            };
            let el = parse(&data).map_err(|msg| IdmlError::Part { part: src.into(), msg })?;
            top.extend(el.elements().cloned());
        } else {
            top.push(c.clone());
        }
    }
    let mut im = Importer::new(read_link, previews);
    if let Some(m) = pkg.part("META-INF/metadata.xml")? {
        let m = String::from_utf8_lossy(&m);
        im.created = xmp_date(&m, "xmp:CreateDate");
        im.modified = xmp_date(&m, "xmp:ModifyDate");
    }
    im.run(&root, &top)?;
    if im.repaired_tab_lists > 0 {
        warnings.push(format!(
            "tab lists repaired to fit the limits (at most {} stops, finite positions up to {} in, leaders of at most {} characters, one stop per position): {}",
            designcraft_doc::MAX_TAB_STOPS,
            designcraft_doc::MAX_TAB_POSITION / 72.0,
            designcraft_doc::MAX_TAB_LEADER,
            im.repaired_tab_lists,
        ));
    }
    warnings.append(&mut im.warnings);
    let mut document = im.finish(&root).map_err(|e| match e {
        // Without its spread parts there is nothing to open: say which parts were missing.
        IdmlError::Invalid(msg) if !warnings.is_empty() => IdmlError::Invalid(format!("{msg} ({})", warnings.join("; "))),
        e => e,
    })?;
    warnings.extend(document.fix_parent_bases());
    document.check().map_err(|e| IdmlError::Invalid(e.to_string()))?;
    Ok(Imported { document, warnings })
}

struct ItemCtx {
    /// Self → (story self, prev self, next self)
    threads: Vec<(ItemId, String, String, String)>,
}

struct Importer<'r> {
    read_link: &'r dyn Fn(&str) -> Option<Vec<u8>>,
    /// Graphic `Self` → stand-in image for a linked file that can't be read.
    previews: &'r HashMap<String, Arc<Vec<u8>>>,
    next_id: u64,
    settings: DocSettings,
    default_margins: Option<(Margins, Columns)>,
    swatches: Vec<Swatch>,
    /// Swatch Self → our swatch name.
    swatch_names: HashMap<String, String>,
    /// Hidden (unnamed) colours by Self.
    hidden_colors: HashMap<String, Color>,
    /// Unnamed swatches (gradients) added on first use.
    hidden_swatches: HashMap<String, Swatch>,
    color_groups: Vec<designcraft_doc::ColorGroup>,
    conditions: Vec<designcraft_doc::Condition>,
    condition_names: HashMap<String, String>,
    inks: designcraft_doc::InkManager,
    stroke_styles: Vec<designcraft_doc::StrokeStyleDef>,
    styles: Styles,
    composite_names: HashMap<String, String>,
    mojikumi_names: HashMap<String, String>,
    kinsoku: HashMap<String, designcraft_doc::cjk::Kinsoku>,
    para_names: HashMap<String, String>,
    char_names: HashMap<String, String>,
    lists: Vec<designcraft_doc::NumberedList>,
    object_names: HashMap<String, String>,
    cell_names: HashMap<String, String>,
    table_names: HashMap<String, String>,
    /// Object style Self → element (attribute fallback for page items).
    object_els: HashMap<String, El>,
    /// Paragraph style Self → element (attribute fallback for partly overridden settings).
    para_els: HashMap<String, El>,
    layers: Vec<Layer>,
    layer_ids: HashMap<String, LayerId>,
    stories: BTreeMap<StoryId, Story>,
    story_ids: HashMap<String, StoryId>,
    parents: Vec<Spread>,
    /// Things read but not kept as they were, for the import report.
    warnings: Vec<String>,
    /// Cells whose text is rotated (Cell Options › Text › Rotation): the angle is kept and
    /// written back, but only 0 is composed, so a narrow header column of vertical labels comes
    /// in horizontal and wraps mid-word (#1468).
    rotated_cells: usize,
    parent_ids: HashMap<String, SpreadId>,
    spreads: Vec<Spread>,
    page_index: HashMap<String, usize>,
    item_ids: HashMap<String, ItemId>,
    assets: BTreeMap<AssetId, Arc<Asset>>,
    /// Assets read from a link path at a page and crop, so later placements of that path, page
    /// and crop reuse them unread.
    linked_assets: HashMap<(String, u32, designcraft_doc::PdfCrop), AssetId>,
    ctx: ItemCtx,
    sections: Vec<Section>,
    footnote_options: designcraft_doc::FootnoteOptions,
    /// Text destination Self → anchor id.
    anchor_ids: HashMap<String, u64>,
    /// Cross-reference source Self → (story, index in its cross-references).
    xref_srcs: HashMap<String, (StoryId, usize)>,
    /// Cross-reference format Self → name.
    xref_format_names: HashMap<String, String>,
    xref_formats: Vec<designcraft_doc::XrefFormat>,
    /// Index topic Self → topic path and per-level sort keys; topic cross-references.
    index_topics: HashMap<String, IndexTopic>,
    index_xrefs: Vec<designcraft_doc::IndexRef>,
    /// The file's text variables (`None`: it defines none) and their Self → index.
    text_vars: Option<Vec<designcraft_doc::vars::TextVariable>>,
    text_var_ids: HashMap<String, usize>,
    /// Creation and modification times (Unix seconds) from the package metadata.
    created: Option<i64>,
    modified: Option<i64>,
    /// Tab lists [`TabStop::sanitized_list`] had to repair (an import warning).
    repaired_tab_lists: usize,
    toc: Option<designcraft_doc::Toc>,
}

impl<'r> Importer<'r> {
    fn new(read_link: &'r dyn Fn(&str) -> Option<Vec<u8>>, previews: &'r HashMap<String, Arc<Vec<u8>>>) -> Self {
        let specials: Vec<Swatch> = designcraft_color::default_swatches().into_iter().filter(|s| s.locked).collect();
        let mut swatch_names = HashMap::new();
        swatch_names.insert("Swatch/None".to_string(), swatch::NONE.to_string());
        swatch_names.insert("Color/Paper".to_string(), swatch::PAPER.to_string());
        swatch_names.insert("Color/Black".to_string(), swatch::BLACK.to_string());
        swatch_names.insert("Color/Registration".to_string(), swatch::REGISTRATION.to_string());
        let mut styles = Styles::default();
        // Built-ins are replaced by the file's definitions when present.
        styles.paragraph.iter_mut().for_each(|s| s.based_on = None);
        Importer {
            read_link,
            previews,
            next_id: 0,
            settings: DocSettings::default(),
            default_margins: None,
            swatches: specials,
            swatch_names,
            hidden_colors: HashMap::new(),
            hidden_swatches: HashMap::new(),
            color_groups: Vec::new(),
            conditions: Vec::new(),
            condition_names: HashMap::new(),
            inks: Default::default(),
            stroke_styles: Vec::new(),
            styles,
            composite_names: HashMap::new(),
            mojikumi_names: HashMap::new(),
            kinsoku: HashMap::new(),
            para_names: HashMap::new(),
            char_names: HashMap::new(),
            lists: Vec::new(),
            object_names: HashMap::new(),
            cell_names: HashMap::new(),
            table_names: HashMap::new(),
            object_els: HashMap::new(),
            para_els: HashMap::new(),
            layers: Vec::new(),
            layer_ids: HashMap::new(),
            stories: BTreeMap::new(),
            story_ids: HashMap::new(),
            parents: Vec::new(),
            warnings: Vec::new(),
            rotated_cells: 0,
            parent_ids: HashMap::new(),
            spreads: Vec::new(),
            page_index: HashMap::new(),
            item_ids: HashMap::new(),
            assets: BTreeMap::new(),
            linked_assets: HashMap::new(),
            ctx: ItemCtx { threads: Vec::new() },
            sections: Vec::new(),
            footnote_options: Default::default(),
            anchor_ids: HashMap::new(),
            xref_srcs: HashMap::new(),
            xref_format_names: HashMap::new(),
            xref_formats: designcraft_doc::xref::default_formats(),
            index_topics: HashMap::new(),
            index_xrefs: Vec::new(),
            text_vars: None,
            text_var_ids: HashMap::new(),
            created: None,
            modified: None,
            repaired_tab_lists: 0,
            toc: None,
        }
    }

    fn alloc(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn run(&mut self, _root: &El, top: &[El]) -> Result<()> {
        // Preferences first (page size, facing pages).
        for e in top {
            match e.local() {
                "DocumentPreference" => self.doc_prefs(e),
                "TransparencyPreference" => {
                    if let Some(v) = e.get("BlendingSpace") {
                        self.settings.blend_space = if v == "RGB" { designcraft_doc::BlendSpace::Rgb } else { designcraft_doc::BlendSpace::Cmyk };
                    }
                }
                "TextPreference" => {
                    let a = &mut self.settings.advanced_type;
                    for (k, v) in [
                        ("SuperscriptSize", &mut a.superscript_size),
                        ("SuperscriptPosition", &mut a.superscript_position),
                        ("SubscriptSize", &mut a.subscript_size),
                        ("SubscriptPosition", &mut a.subscript_position),
                    ] {
                        if let Some(x) = e.num(k) {
                            *v = x;
                        }
                    }
                }
                "MarginPreference" => {
                    let (m, c) = margins_of(e);
                    self.default_margins = Some((m, c));
                }
                "ViewPreference" => {
                    if let Some(u) = e.get("HorizontalMeasurementUnits") {
                        self.settings.horizontal_units = names::unit_in(u);
                    }
                    if let Some(u) = e.get("VerticalMeasurementUnits") {
                        self.settings.vertical_units = names::unit_in(u);
                    }
                    if let Some(v) = e.num("CursorKeyIncrement") {
                        self.settings.keyboard_increment = v;
                    }
                    match e.get("RulerOrigin") {
                        Some("PageOrigin") => self.settings.ruler_origin = designcraft_doc::RulerOrigin::Page,
                        Some("SpineOrigin") => self.settings.ruler_origin = designcraft_doc::RulerOrigin::Spine,
                        Some("SpreadOrigin") => self.settings.ruler_origin = designcraft_doc::RulerOrigin::Spread,
                        _ => {}
                    }
                }
                "GridPreference" => self.grid_prefs(e),
                "PasteboardPreference" => {
                    if let Some(v) = e.get("PasteboardMargins").map(nums)
                        && v.len() == 2
                        && v[0] >= 0.0
                        && v[1] >= 0.0
                    {
                        self.settings.pasteboard = (v[0], v[1]);
                    }
                }
                _ => {}
            }
        }
        for e in top.iter().filter(|e| e.local() == "CompositeFont") {
            let name =
                e.get("Name").map(str::to_string).unwrap_or_else(|| unescape_id(e.get("Self").unwrap_or("").trim_start_matches("CompositeFont/")));
            if let Some(id) = e.get("Self") {
                self.composite_names.insert(id.to_string(), format!("CompositeFont/{name}"));
            }
            let entries = e
                .find_all("CompositeFontEntry")
                .map(|c| designcraft_doc::cjk::CompositeFontEntry {
                    name: c.get("Name").unwrap_or("").into(),
                    characters: c.get("CustomCharacters").unwrap_or("").into(),
                    family: c.prop("AppliedFont").as_deref().and_then(names::font_family_in).unwrap_or_default(),
                    style: c.get("FontStyle").unwrap_or("Regular").trim_start_matches("$ID/").into(),
                    // A metric `Document::check` refuses would make the whole file fail to
                    // open, so one that is unusable here falls back to full size / no shift.
                    relative_size: c.num("RelativeSize").map(|v| v / 100.0).filter(|v| v.is_finite() && *v > 0.0).unwrap_or(1.0),
                    horizontal_scale: c.num("HorizontalScale").map(|v| v / 100.0).filter(|v| v.is_finite() && *v > 0.0).unwrap_or(1.0),
                    vertical_scale: c.num("VerticalScale").map(|v| v / 100.0).filter(|v| v.is_finite() && *v > 0.0).unwrap_or(1.0),
                    baseline_shift: c.num("BaselineShift").filter(|v| v.is_finite()).unwrap_or(0.0) / 100.0,
                    scale_option: c.boolean("ScaleOption").unwrap_or(true),
                })
                .collect();
            self.styles.composite_fonts.push(designcraft_doc::cjk::CompositeFont { name, entries });
        }
        for e in top.iter().filter(|e| e.local() == "KinsokuTable") {
            if e.get("CantBeginLineChars").is_some() || e.get("CantEndLineChars").is_some() {
                self.kinsoku.insert(
                    e.get("Self").unwrap_or("").into(),
                    designcraft_doc::cjk::Kinsoku {
                        name: e.get("Name").unwrap_or("").into(),
                        no_start: e.get("CantBeginLineChars").unwrap_or("").into(),
                        no_end: e.get("CantEndLineChars").unwrap_or("").into(),
                        inseparable: e.get("CantBeSeparatedChars").unwrap_or("").into(),
                        hanging: e.get("HangingPunctuationChars").unwrap_or("").into(),
                    },
                );
            }
        }
        for e in top.iter().filter(|e| e.local() == "MojikumiTable") {
            if let (Some(id), Some(name)) = (e.get("Self"), e.get("Name")) {
                self.mojikumi_names.insert(id.into(), format!("MojikumiTable/{}", unescape_id(name)));
            }
            let overrides = if let Some(list) = e.prop_el("OverrideMojikumiAkiList") {
                list.find_all("OverrideMojikumiAkiType").map(mojikumi_row).collect::<Result<Vec<_>>>()?
            } else {
                Vec::new()
            };
            self.styles.mojikumi_tables.push(designcraft_doc::cjk::MojikumiTable {
                name: unescape_id(e.get("Name").unwrap_or("")),
                based_on: e.get("BasedOnMojikumiSet").unwrap_or("").into(),
                overrides,
            });
        }
        self.graphics(top);
        self.styles(top);
        if let Some(e) = top.iter().find(|e| e.local() == "FootnoteOption") {
            self.footnote_option(e);
        }
        if let Some(e) = top.iter().find(|e| e.local() == "EPubExportPreference") {
            self.epub_split(e);
        }
        for e in top.iter().filter(|e| e.local() == "Layer") {
            let id = LayerId(self.alloc());
            let color = match e.prop_el("LayerColor") {
                Some(c) if c.get("type") == Some("list") => {
                    let v: Vec<f64> = c.find_all("ListItem").filter_map(|i| i.text_content().trim().parse().ok()).collect();
                    if v.len() == 3 { [v[0] as u8, v[1] as u8, v[2] as u8] } else { LAYER_COLORS[0].1 }
                }
                Some(c) => names::layer_color_in(c.text_content().trim()).unwrap_or(LAYER_COLORS[self.layers.len() % LAYER_COLORS.len()].1),
                None => LAYER_COLORS[self.layers.len() % LAYER_COLORS.len()].1,
            };
            self.layers.push(Layer {
                id,
                name: e.get("Name").unwrap_or("Layer").to_string(),
                color,
                visible: e.get("Visible") != Some("false"),
                locked: e.get("Locked") == Some("true"),
                printable: e.get("Printable") != Some("false"),
                show_guides: e.get("ShowGuides") != Some("false"),
                suppress_wrap_when_hidden: e.get("IgnoreWrap") == Some("true"),
            });
            if let Some(s) = e.get("Self") {
                self.layer_ids.insert(s.to_string(), id);
            }
        }
        // IDML lists layers back to front; `Document::layers` is front to back.
        self.layers.reverse();
        if self.layers.is_empty() {
            let id = LayerId(self.alloc());
            self.layers.push(Layer {
                id,
                name: "Layer 1".into(),
                color: LAYER_COLORS[0].1,
                visible: true,
                locked: false,
                printable: true,
                show_guides: true,
                suppress_wrap_when_hidden: false,
            });
        }
        for e in top.iter().filter(|e| e.local() == "CrossReferenceFormat") {
            self.xref_format(e);
        }
        if let Some(ix) = top.iter().find(|e| e.local() == "Index") {
            self.index_topics_of(ix, &IndexTopic::default());
        }
        for e in top.iter().filter(|e| e.local() == "TextVariable") {
            self.text_variable(e);
        }
        // Stories (ids first so frames can reference them).
        let mut pending_stories = Vec::new();
        for e in top.iter().filter(|e| e.local() == "Story") {
            let id = StoryId(self.alloc());
            if let Some(s) = e.get("Self") {
                self.story_ids.insert(s.to_string(), id);
            }
            pending_stories.push((id, e));
        }
        for (id, e) in pending_stories {
            let story = self.story(id, e);
            self.stories.insert(id, story);
        }
        self.toc = self.toc_of(top);
        // Topic cross-references (See / See also) become markers at the start of the first story
        // with text.
        if !self.index_xrefs.is_empty()
            && let Some(st) = self.stories.values_mut().find(|s| !s.text.is_empty())
        {
            for r in std::mem::take(&mut self.index_xrefs).into_iter().rev() {
                st.insert_index_ref(0, r);
            }
        }
        // Cross-references point at their hyperlink's destination.
        for h in top.iter().filter(|e| e.local() == "Hyperlink") {
            let (Some(src), Some(dest)) = (h.get("Source"), h.prop("Destination")) else { continue };
            let (Some(&(sid, k)), Some(&target)) = (self.xref_srcs.get(src), self.anchor_ids.get(dest.trim())) else { continue };
            if let Some(x) = self.stories.get_mut(&sid).and_then(|st| st.xrefs.get_mut(k)) {
                Arc::make_mut(x).target = target;
            }
        }
        // Parent spreads: ids first (pages reference other parents).
        let masters: Vec<&El> = top.iter().filter(|e| e.local() == "MasterSpread").collect();
        let mut master_ids = Vec::new();
        for m in &masters {
            let id = SpreadId(self.alloc());
            if let Some(s) = m.get("Self") {
                self.parent_ids.insert(s.to_string(), id);
            }
            master_ids.push(id);
        }
        for (m, id) in masters.iter().zip(master_ids) {
            let sp = self.spread(m, id, true);
            self.parents.push(sp);
        }
        for e in top.iter().filter(|e| e.local() == "Spread") {
            let id = SpreadId(self.alloc());
            let sp = self.spread(e, id, false);
            self.spreads.push(sp);
        }
        // Sections.
        for e in top.iter().filter(|e| e.local() == "Section") {
            let Some(start) = e.get("PageStart").and_then(|p| self.page_index.get(p)).copied() else { continue };
            let cont = e.get("ContinueNumbering") != Some("false");
            self.sections.push(Section {
                start,
                start_number: if cont && start > 0 { None } else { Some(e.num("PageNumberStart").unwrap_or(1.0).max(1.0) as u32) },
                style: e.prop("PageNumberStyle").map(|s| names::number_style_in(s.trim())).unwrap_or_default(),
                prefix: e.get("SectionPrefix").unwrap_or("").to_string(),
                marker: e.get("Marker").unwrap_or("").to_string(),
                include_prefix: e.get("IncludeSectionPrefix") == Some("true"),
            });
        }
        if self.rotated_cells > 0 {
            let n = self.rotated_cells;
            self.warnings.push(format!(
                "{n} table cell(s) have rotated text (Cell Options › Text › Rotation): the angle is kept and written back, but the text is composed horizontally"
            ));
        }
        self.sections.sort_by_key(|s| s.start);
        self.sections.dedup_by_key(|s| s.start);
        if self.sections.first().is_none_or(|s| s.start != 0) {
            self.sections.insert(
                0,
                Section {
                    start: 0,
                    start_number: Some(1),
                    style: Default::default(),
                    prefix: String::new(),
                    marker: String::new(),
                    include_prefix: false,
                },
            );
        }
        self.link_threads();
        Ok(())
    }

    fn finish(mut self, root: &El) -> Result<Document> {
        let title = root.get("Name").map(|n| n.trim_end_matches(".indd").to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "Untitled".into());
        if self.spreads.is_empty() {
            return Err(IdmlError::Invalid("document has no spreads".into()));
        }
        self.settings.lists = std::mem::take(&mut self.lists);
        self.clear_inherited_drop_cap_styles();
        // Make sure built-in paragraph styles exist and are first.
        let mut d = Document {
            title,
            file_info: Default::default(),
            settings: self.settings.clone(),
            spreads: std::mem::take(&mut self.spreads).into_iter().map(Arc::new).collect(),
            parents: std::mem::take(&mut self.parents).into_iter().map(Arc::new).collect(),
            layers: std::mem::take(&mut self.layers),
            stories: std::mem::take(&mut self.stories).into_iter().map(|(k, v)| (k, Arc::new(v))).collect(),
            styles: Arc::new(std::mem::take(&mut self.styles)),
            swatches: std::mem::take(&mut self.swatches),
            color_groups: std::mem::take(&mut self.color_groups),
            conditions: std::mem::take(&mut self.conditions),
            inks: std::mem::take(&mut self.inks),
            stroke_styles: std::mem::take(&mut self.stroke_styles),
            articles: vec![],
            xml: Default::default(),
            endnote_options: Default::default(),
            endnote_story: None,
            sections: std::mem::take(&mut self.sections),
            assets: std::mem::take(&mut self.assets),
            hyperlinks: vec![],
            data_merge: Default::default(),
            bookmarks: vec![],
            user_words: vec![],
            hyphenation_exceptions: vec![],
            toc: self.toc.take(),
            text_variables: self.text_vars.take().unwrap_or_else(designcraft_doc::vars::defaults),
            footnote_options: std::mem::take(&mut self.footnote_options),
            xref_formats: std::mem::take(&mut self.xref_formats),
            index: None,
            created: self.created.unwrap_or_else(designcraft_doc::vars::now),
            modified: self.modified.unwrap_or(0),
            next_id: self.next_id,
            font_scope: 0,
        };
        let vars = self.text_vars.get_or_insert_with(Vec::new);
        if let Some(s) = e.get("Self") {
            self.text_var_ids.insert(s.to_string(), vars.len());
        }
        vars.push(v);
    }

    /// A `TextVariableInstance`: the variable's character, or its recorded text when the file
    /// doesn't define the variable.
    fn text_variable_instance(&mut self, e: &El) -> String {
        use designcraft_doc::vars::{VarKind, var_char};
        let result = e.get("ResultText").unwrap_or("");
        let found = e.get("AssociatedTextVariable").and_then(|r| self.text_var_ids.get(r)).copied();
        let var = found.and_then(|i| Some((i, self.text_vars.as_mut()?.get_mut(i)?)));
        match var.and_then(|(i, v)| Some((var_char(i)?, v))) {
            Some((c, v)) => {
                if let VarKind::Imported { result: r, .. } = &mut v.kind
                    && r.is_empty()
                {
                    *r = result.to_string();
                }
                c.to_string()
            }
            None => result.to_string(),
        }
    }

    fn xref_format(&mut self, e: &El) {
        let name = e.get("Name").unwrap_or("Format").to_string();
        let mut def = String::new();
        for b in e.find_all("BuildingBlock") {
            let text = |k: &str| b.get(k).filter(|v| *v != "$ID/").unwrap_or("").to_string();
            match b.get("BlockType").unwrap_or("") {
                "CustomStringBuildingBlock" => def.push_str(&text("CustomText")),
                "FullParagraphBuildingBlock" => def.push_str("<fullPara />"),
                "ParagraphTextBuildingBlock" => def.push_str("<paraText />"),
                "ParagraphNumberBuildingBlock" => def.push_str("<paraNum />"),
                "PageNumberBuildingBlock" => def.push_str("<pageNum />"),
                "BookmarkNameBuildingBlock" => def.push_str("<txtAnchrName />"),
                "ChapterNumberBuildingBlock" => def.push_str("<chapNum />"),
                "FileNameBuildingBlock" => def.push_str("<fileName />"),
                "PartialParagraphBuildingBlock" => def.push_str(&format!(
                    "<partialPara delim=\"{}\" includeDelim=\"{}\" />",
                    text("AppliedDelimiter"),
                    b.get("IncludeDelimiter") == Some("true")
                )),
                _ => {}
            }
        }
        if let Some(me) = e.get("Self") {
            self.xref_format_names.insert(me.to_string(), name.clone());
        }
        match self.xref_formats.iter_mut().find(|f| f.name == name) {
            Some(f) => f.definition = def,
            None => self.xref_formats.push(designcraft_doc::XrefFormat { name, definition: def }),
        }
    }

    /// Preferences › `FootnoteOption` (after styles: it names the footnote text and marker styles).
    fn footnote_option(&mut self, e: &El) {
        let mut o = designcraft_doc::FootnoteOptions::default();
        if let Some(v) = e.num("StartAt") {
            o.start_at = v.max(0.0) as u32;
        }
        o.prefix = e.get("Prefix").unwrap_or("").into();
        o.suffix = e.get("Suffix").unwrap_or("").into();
        if let Some(r) = e.get("FootnoteTextStyle") {
            o.para_style = self.para_style_ref(r);
        }
        if let Some(r) = e.get("FootnoteMarkerStyle") {
            o.ref_char_style = self.char_style_ref(r);
        }
        if let Some(v) = e.get("SeparatorText") {
            o.separator = v.into();
        }
        o.space_between = e.num("SpaceBetween").unwrap_or(0.0);
        o.space_before = e.num("Spacer").unwrap_or(0.0);
        if let Some(v) = e.prop("FootnoteFirstBaselineOffset") {
            o.first_baseline = names::first_baseline_in(v.trim());
        }
        o.first_baseline_min = e.num("FootnoteMinimumFirstBaselineOffset").unwrap_or(0.0);
        o.span_columns = e.boolean("EnableStraddling").unwrap_or(false);
        o.rule.on = e.boolean("RuleOn").unwrap_or(true);
        o.rule.weight = e.num("RuleLineWeight").unwrap_or(1.0);
        o.rule.tint = tint(e.num("RuleTint")).unwrap_or(1.0);
        o.rule.left_indent = e.num("RuleLeftIndent").unwrap_or(0.0);
        o.rule.width = e.num("RuleWidth").unwrap_or(72.0);
        o.rule.offset = e.num("RuleOffset").unwrap_or(0.0);
        if let Some(c) = e.prop("RuleColor") {
            o.rule.color = self.swatch_ref(c.trim());
        }
        if let Some(v) = e.prop("FootnoteNumberingStyle") {
            o.style = names::note_style_in(v.trim());
        }
        if let Some(v) = e.prop("RestartNumbering") {
            o.restart = names::NOTE_RESTART.iter().find(|r| r.1 == v.trim()).map_or(Default::default(), |r| r.0);
        }
        if let Some(v) = e.prop("ShowPrefixSuffix") {
            o.affix_in = names::NOTE_AFFIX.iter().find(|r| r.1 == v.trim()).map_or(Default::default(), |r| r.0);
        }
        if let Some(v) = e.prop("MarkerPositioning") {
            o.ref_position = names::note_marker_in(v.trim());
        }
        self.footnote_options = o;
    }

    /// The table of contents: the TOC style a story is generated from (`AppliedTOCStyle`), with
    /// its entries. Styles the document doesn't have fall back to the generated TOC styles.
    fn toc_of(&self, top: &[El]) -> Option<designcraft_doc::Toc> {
        use designcraft_doc::{TocEntry, TocPageNumber};
        let (ts, story) = top.iter().filter(|e| e.local() == "TOCStyle").find_map(|ts| {
            let id = ts.get("Self")?;
            let story = top.iter().find(|e| e.local() == "Story" && e.get("AppliedTOCStyle") == Some(id))?;
            Some((ts, *self.story_ids.get(story.get("Self")?)?))
        })?;
        let known = |r: Option<&str>| r.map(|r| self.para_style_ref(r)).filter(|n| self.styles.para(n).is_some());
        // Character styles the document has; [None] is stored as no style.
        let known_chars = |r: Option<&str>| {
            r.map(|r| self.char_style_ref(r)).filter(|n| n != designcraft_doc::NO_CHAR_STYLE && self.styles.char_style(n).is_some())
        };
        let entries: Vec<TocEntry> = ts
            .elements()
            .filter(|e| e.local() == "TOCStyleEntry")
            .filter_map(|e| {
                let style = names::style_name_in(PARA_BUILTINS, e.get("Name")?);
                if style.is_empty() || style.chars().count() > 256 {
                    return None;
                }
                let level = e.get("Level").and_then(|l| l.trim().parse::<u8>().ok()).unwrap_or(1).clamp(1, 9);
                let page_number = match e.get("PageNumberPosition") {
                    Some("BeforeEntry") => TocPageNumber::Before,
                    Some("NoPageNumber") => TocPageNumber::None,
                    _ => TocPageNumber::After,
                };
                let separator = e.get("Separator").map(crate::names::toc_separator_in).filter(|s| s != "\t" && s.chars().count() <= 64);
                let entry_style = known(e.get("FormatStyle")).filter(|n| *n != format!("TOC Level {level}"));
                let page_number_style = known_chars(e.get("PageNumberStyle"));
                let separator_style = known_chars(e.get("SeparatorStyle"));
                let sort = e.get("Sort").is_some_and(|v| v.trim() == "true");
                Some(TocEntry { style, level, entry_style, page_number, separator, page_number_style, separator_style, sort })
            })
            .take(64)
            .collect();
        if entries.is_empty() {
            return None;
        }
        let title: String = ts.get("Title").unwrap_or("").chars().filter(|c| !matches!(c, '\n' | '\r' | '\u{2029}')).take(1024).collect();
        let title_style = known(ts.get("TitleStyle")).filter(|n| n != "TOC Title");
        Some(designcraft_doc::Toc { story, title, entries, page_numbers: true, title_style })
    }

    fn para_style_ref(&self, r: &str) -> String {
        self.para_names.get(r).cloned().unwrap_or_else(|| names::style_name_in(PARA_BUILTINS, &unescape_id(r.trim_start_matches("ParagraphStyle/"))))
    }
    /// A character style reference written as an element's text (nested, line and GREP styles).
    /// A style name may end in a space, so the reference is used as written when it names a
    /// style; only otherwise is surrounding whitespace dropped.
    fn listed_char_style_ref(&self, r: &str) -> String {
        if self.char_names.contains_key(r) { self.char_style_ref(r) } else { self.char_style_ref(r.trim()) }
    }

    fn char_style_ref(&self, r: &str) -> String {
        self.char_names.get(r).cloned().unwrap_or_else(|| names::style_name_in(CHAR_BUILTINS, &unescape_id(r.trim_start_matches("CharacterStyle/"))))
    }

    fn char_attrs(&mut self, e: &El) -> CharAttrs {
        let mut a = CharAttrs::default();
        if let Some(f) = e.prop("AppliedFont") {
            // Some writers append the style after a tab. Spaces are part of a family's name
            // ("Minion Pro " is another font than "Minion Pro"); line breaks around it aren't.
            let fam = f.split('\t').next().unwrap_or(&f).trim_matches(['\n', '\r']);
            a.font_family = self.composite_names.get(fam.trim()).cloned().or_else(|| names::font_family_in(fam));
        }
        // "Nothing" leaves the style unset: it comes from the paragraph style.
        a.font_style = e.prop("FontStyle").filter(|s| !matches!(s.trim(), "Nothing" | "$ID/Nothing"));
        a.size = e.num("PointSize");
        a.leading = e.prop("Leading").and_then(|l| names::leading_in(l.trim()));
        // Automatic kerning may carry an inactive numeric value (including the
        // 1e11 IDML sentinel). It must not become a multi-million-em advance.
        a.kerning = e.prop("KerningMethod").and_then(|k| match k.trim().trim_start_matches("$ID/") {
            "Optical" => Some(Kerning::Optical),
            "Metrics" | "Metrics - Roman Only" => Some(Kerning::Metrics),
            "None" => Some(Kerning::None),
            _ => None,
        });
        if a.kerning.is_none() {
            a.kerning = e.num("KerningValue").filter(|v| v.is_finite() && v.abs() < 1e10).map(Kerning::Manual);
        }
        a.tracking = e.num("Tracking");
        a.leading_aki = e.num("LeadingAki").map(|v| (v >= 0.0).then_some(v));
        a.trailing_aki = e.num("TrailingAki").map(|v| (v >= 0.0).then_some(v));
        a.tsume = e.num("Tsume").map(|v| (v / 100.0).clamp(0.0, 1.0));
        a.jidori = e.num("Jidori").map(|v| v.clamp(0.0, 10000.0) as u32);
        a.leading_model = e.prop("LeadingModel").and_then(|v| crate::cjk::leading_in(&v));
        a.character_alignment = e.prop("CharacterAlignment").and_then(|v| crate::cjk::alignment_in(&v));
        a.h_scale = e.num("HorizontalScale").map(|v| v / 100.0);
        a.v_scale = e.num("VerticalScale").map(|v| v / 100.0);
        a.baseline_shift = e.num("BaselineShift");
        a.skew = e.num("Skew");
        if let Some(r) = e.prop("FillColor") {
            a.fill = Some(self.swatch_ref(r.trim()));
        }
        a.fill_tint = tint(e.num("FillTint"));
        if let Some(r) = e.prop("StrokeColor") {
            a.stroke = Some(self.swatch_ref(r.trim()));
        }
        a.stroke_tint = tint(e.num("StrokeTint"));
        a.stroke_weight = e.num("StrokeWeight");
        // Text strokes are centred on the outline or lie outside it, behind the fill.
        a.stroke_behind_fill = e.prop("StrokeAlignment").and_then(|v| match v.trim() {
            "OutsideAlignment" => Some(true),
            "CenterAlignment" => Some(false),
            _ => None,
        });
        a.capitalization = e.prop("Capitalization").and_then(|v| names::caps_in(v.trim()));
        a.position = e.prop("Position").and_then(|v| names::position_in(v.trim()));
        a.underline = e.boolean("Underline");
        a.strikethrough = e.boolean("StrikeThru");
        a.ligatures = e.boolean("Ligatures");
        // Underline / strikethrough options (-9999 = automatic).
        for k in ["Underline", "StrikeThru"] {
            let num = |n: &str| e.num(&format!("{k}{n}")).filter(|v| *v > -9000.0);
            // An explicit Text Color resets an inherited swatch; absence still inherits.
            // Resolve the sentinel before swatch_ref, which maps unknown names to [None].
            let color = e.prop(&format!("{k}Color")).map(|r| if r.trim() == "Text Color" { String::new() } else { self.swatch_ref(r.trim()) });
            let tint = tint(e.num(&format!("{k}Tint")));
            if k == "Underline" {
                a.underline_weight = num("Weight").map(Some);
                a.underline_offset = num("Offset").map(Some);
                a.underline_color = color;
                a.underline_tint = tint;
            } else {
                a.strikethrough_weight = num("Weight").map(Some);
                a.strikethrough_offset = num("Offset").map(Some);
                a.strikethrough_color = color;
                a.strikethrough_tint = tint;
            }
        }
        read_otf_features(e, &mut a);
        a.glyph_form = e.prop("GlyphForm");
        a.no_break = e.boolean("NoBreak");
        a.tate_chu_yoko = e.boolean("Tatechuyoko");
        a.tate_chu_yoko_x_offset = e.num("TatechuyokoXOffset");
        a.tate_chu_yoko_y_offset = e.num("TatechuyokoYOffset");
        a.warichu = e.boolean("Warichu");
        a.warichu_lines = e.num("WarichuLines").map(|v| v.max(2.0) as u32);
        a.warichu_size = e.num("WarichuSize");
        a.warichu_line_spacing = e.num("WarichuLineSpacing");
        a.warichu_alignment = e.prop("WarichuAlignment").as_deref().and_then(crate::cjk::warichu_align_in);
        a.warichu_chars_before_break = e.num("WarichuCharsBeforeBreak").map(|v| v.max(0.0) as u32);
        a.warichu_chars_after_break = e.num("WarichuCharsAfterBreak").map(|v| v.max(0.0) as u32);
        // InDesign writes the flag as a number (1); older writers wrote a boolean.
        if let Some(on) = e.boolean("RubyFlag").or_else(|| e.num("RubyFlag").map(|v| v != 0.0)) {
            a.ruby = Some(if on { e.prop("RubyString").unwrap_or_default() } else { String::new() });
        }
        crate::ruby::read(e, &mut a, |r| self.swatch_ref(r));
        if let Some(k) = e.get("KentenKind") {
            a.kenten = Some(k != "None");
            a.kenten_character = crate::cjk::kenten_character(k).map(str::to_string);
            if k == "Custom" {
                a.kenten_character = e.prop("KentenCustomCharacter");
            }
        }
        a.digits = e.prop("DigitsType").and_then(|s| names::digits_in(s.trim()));
        a.character_direction = e.prop("CharacterDirection").and_then(|s| crate::arabic::direction_in(&s));
        a.allow_kashidas = e.prop("Kashidas").and_then(|s| crate::arabic::kashidas_in(&s));
        a.diacritic_position = e.prop("DiacriticPosition").and_then(|s| crate::arabic::diacritic_in(&s));
        a.diacritic_x_offset = e.num("XOffsetDiacritic").filter(|v| v.is_finite());
        a.diacritic_y_offset = e.num("YOffsetDiacritic").filter(|v| v.is_finite());
        a.positional_form = e.prop("PositionalForm");
        self.cjk_settings_chars(e, &mut a);
        if let Some(v) = e.get("AppliedConditions").filter(|v| !v.trim().is_empty()) {
            let names: Vec<String> = v.split_whitespace().filter_map(|r| self.condition_names.get(r).cloned()).collect();
            if !names.is_empty() {
                a.conditions = Some(names);
            }
        }
        a.language = e.prop("AppliedLanguage").map(|l| l.trim().trim_start_matches("$ID/").to_string());
        a
    }

    fn para_attrs(&mut self, e: &El) -> ParaAttrs {
        let mut a = ParaAttrs::default();
        let u = |k: &str| e.num(k).map(|v| v.max(0.0) as u32);
        let frac = |k: &str| e.num(k).map(|v| v / 100.0);
        a.align = e.prop("Justification").and_then(|v| names::align_in(v.trim()));
        a.kinsoku_hang = e.prop("KinsokuHangType").and_then(|v| match v.as_str() {
            "None" => Some(designcraft_doc::cjk::KinsokuHang::None),
            "KinsokuHangRegular" => Some(designcraft_doc::cjk::KinsokuHang::Regular),
            "KinsokuHangForce" => Some(designcraft_doc::cjk::KinsokuHang::Force),
            _ => None,
        });
        a.mojikumi = e.prop("Mojikumi").map(|name| self.mojikumi_names.get(&name).cloned().unwrap_or_else(|| unescape_id(&name)));
        a.kinsoku_type = e.prop("KinsokuType");
        a.bunri_kinshi = e.boolean("BunriKinshi");
        a.rensuuji = e.boolean("Rensuuji");
        a.treat_ideographic_space_as_space = e.boolean("TreatIdeographicSpaceAsSpace");
        cjk_settings_para(e, &mut a);
        if let Some(k) = e.prop("KinsokuSet") {
            if k == "Nothing" || k == "None" {
                a.kinsoku = Some(Some(Default::default()));
            } else if let Some(set) = self.kinsoku.get(&k) {
                a.kinsoku = Some(Some(set.clone()));
            } else if let Some(set) = designcraft_doc::cjk::Kinsoku::named(&k) {
                a.kinsoku = Some(Some(set));
            }
        }
        a.direction = e.prop("ParagraphDirection").map(|v| {
            if v.trim() == "RightToLeftDirection" { designcraft_doc::TextDirection::RightToLeft } else { designcraft_doc::TextDirection::LeftToRight }
        });
        a.kashidas = e.prop("Kashidas").and_then(|s| crate::arabic::kashidas_in(&s));
        a.arabic_justification = e.prop("ParagraphJustification");
        a.paragraph_kashida_width = e.num("ParagraphKashidaWidth").filter(|v| v.is_finite()).map(Some);
        a.left_indent = e.num("LeftIndent");
        a.right_indent = e.num("RightIndent");
        a.first_line_indent = e.num("FirstLineIndent");
        a.last_line_indent = e.num("LastLineIndent");
        a.space_before = e.num("SpaceBefore");
        if let Some(r) = e.prop("AppliedNumberingList") {
            let r = r.trim().trim_start_matches("NumberingList/");
            a.list_name = Some(if r.starts_with("$ID/") { String::new() } else { unescape_id(r) });
        }
        match e.prop("NumberingContinue").as_deref().map(str::trim) {
            Some("false") => a.start_at = Some(e.num("NumberingStartAt").map(|n| n.max(1.0) as u32)),
            Some("true") => a.start_at = Some(None),
            _ => {}
        }
        // Nested and GREP styles.
        if let Some(l) = e.prop_el("AllNestedStyles") {
            let v: Vec<designcraft_doc::NestedStyle> = l
                .find_all("ListItem")
                .map(|it| {
                    let t = |k: &str| it.find(k).map(|x| x.text_content()).unwrap_or_default();
                    let d = it.find("Delimiter");
                    designcraft_doc::NestedStyle {
                        style: self.listed_char_style_ref(&t("AppliedCharacterStyle")),
                        through: t("Inclusive").trim() != "false",
                        count: t("Repetition").trim().parse().unwrap_or(1),
                        until: names::nested_until_in(d.and_then(|d| d.get("type")).unwrap_or("enumeration"), &t("Delimiter")),
                    }
                })
                .collect();
            // IDML has no drop cap style attribute: the drop cap's character style is the list's
            // leading "through 1 Dropcap" nested style.
            a.drop_cap_style = v.first().filter(|ns| ns.is_drop_cap()).map(|ns| ns.style.clone());
            a.nested_styles = Some(v);
        }
        if let Some(l) = e.prop_el("AllNestedLineStyles") {
            let v: Vec<designcraft_doc::NestedLineStyle> = l
                .find_all("ListItem")
                .map(|it| {
                    let t = |k: &str| it.find(k).map(|x| x.text_content()).unwrap_or_default();
                    designcraft_doc::NestedLineStyle {
                        style: self.listed_char_style_ref(&t("AppliedCharacterStyle")),
                        lines: t("LineCount").trim().parse().unwrap_or(1),
                    }
                })
                .collect();
            a.nested_line_styles = Some(v);
        }
        if let Some(l) = e.prop_el("AllGREPStyles") {
            let v: Vec<designcraft_doc::GrepStyle> = l
                .find_all("ListItem")
                .map(|it| {
                    let t = |k: &str| it.find(k).map(|x| x.text_content()).unwrap_or_default();
                    designcraft_doc::GrepStyle { style: self.listed_char_style_ref(&t("AppliedCharacterStyle")), pattern: t("GrepExpression") }
                })
                .collect();
            a.grep_styles = Some(v);
        }
        a.space_after = e.num("SpaceAfter");
        // A number, or an enumeration (Ignore) that turns it off even over a based-on style's value.
        a.same_para_style_spacing = e.prop("SameParaStyleSpacing").map(|v| v.trim().parse::<f64>().ok().filter(|v| v.is_finite()));
        a.drop_cap_lines = u("DropCapLines");
        a.drop_cap_chars = u("DropCapCharacters");
        // DropcapDetail bits: 1 = Align Left Edge, 2 = Scale for Descenders.
        if let Some(v) = e.num("DropcapDetail").filter(|v| v.is_finite() && *v >= 0.0) {
            let bits = v as u32;
            a.drop_cap_align_left = Some(bits & 1 != 0);
            a.drop_cap_scale_descenders = Some(bits & 2 != 0);
        }
        if let Some(g) = e.prop("GridAlignment") {
            a.grid_align = Some(if g.trim() == "None" {
                GridAlign::None
            } else if e.boolean("GridAlignFirstLineOnly") == Some(true) {
                GridAlign::FirstLineOnly
            } else {
                GridAlign::AllLines
            });
            if g.trim() != "None" {
                a.grid_reference = Some(crate::cjk::grid_reference_in(&g));
            }
        }
        a.composer = e.prop("Composer").map(|c| composer_in(&c));
        a.hyphenate = e.boolean("Hyphenation");
        a.hyph_min_word = u("HyphenateWordsLongerThan");
        a.hyph_after_first = u("HyphenateAfterFirst");
        a.hyph_before_last = u("HyphenateBeforeLast");
        a.hyph_limit = u("HyphenateLadderLimit");
        a.hyph_zone = e.num("HyphenationZone");
        a.hyph_capitalized = e.boolean("HyphenateCapitalizedWords");
        a.hyph_last_word = e.boolean("HyphenateLastWord");
        a.hyph_across_column = e.boolean("HyphenateAcrossColumns");
        a.hyph_weight = e.num("HyphenWeight").map(|v| v / 10.0);
        a.word_space_min = frac("MinimumWordSpacing");
        a.word_space_desired = frac("DesiredWordSpacing");
        a.word_space_max = frac("MaximumWordSpacing");
        a.letter_space_min = frac("MinimumLetterSpacing");
        a.letter_space_desired = frac("DesiredLetterSpacing");
        a.letter_space_max = frac("MaximumLetterSpacing");
        a.glyph_scale_min = frac("MinimumGlyphScaling");
        a.glyph_scale_desired = frac("DesiredGlyphScaling");
        a.glyph_scale_max = frac("MaximumGlyphScaling");
        a.auto_leading = frac("AutoLeading");
        a.single_word_justify = e.prop("SingleWordJustification").and_then(|v| names::align_in(v.trim()));
        a.keep_with_next = u("KeepWithNext");
        a.keep_lines_together = e.boolean("KeepLinesTogether");
        a.ignore_optical_margin = e.boolean("IgnoreEdgeAlignment");
        a.keep_all_lines = e.boolean("KeepAllLinesTogether");
        a.keep_first = u("KeepFirstLines");
        a.keep_last = u("KeepLastLines");
        a.start_paragraph = e.prop("StartParagraph").and_then(|v| names::start_para_in(v.trim()));
        if let Some(t) = e.prop("SpanColumnType") {
            let n = e.prop("SpanSplitColumnCount").and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(0);
            a.span_columns = Some(match t.trim() {
                "SpanColumns" => SpanColumns::Span(n),
                "SplitColumns" => SpanColumns::Split(n.max(2)),
                _ => SpanColumns::Single,
            });
        }
        a.split_inside_gutter = e.num("SplitColumnInsideGutter").filter(|v| v.is_finite());
        a.split_outside_gutter = e.num("SplitColumnOutsideGutter").filter(|v| v.is_finite());
        a.rule_above = self.rule(e, "RuleAbove");
        a.rule_below = self.rule(e, "RuleBelow");
        if let Some(tl) = e.prop_el("TabList") {
            let mut tabs = Vec::new();
            for li in tl.find_all("ListItem") {
                let get = |k: &str| li.find(k).map(|x| x.text_content());
                tabs.push(TabStop {
                    position: get("Position").and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite()).unwrap_or(0.0),
                    align: names::tab_align_in(get("Alignment").unwrap_or_default().trim()),
                    leader: get("Leader").unwrap_or_default(),
                    align_on: get("AlignmentCharacter").filter(|c| c != ".").unwrap_or_default(),
                });
            }
            let sanitized = TabStop::sanitized_list(tabs.clone());
            if tab_list_repaired(&tabs, &sanitized) {
                self.repaired_tab_lists = self.repaired_tab_lists.saturating_add(1);
            }
            a.tabs = Some(sanitized);
        }
        a.list_type = e.prop("BulletsAndNumberingListType").map(|v| match v.trim() {
            "BulletList" => ListType::Bullets,
            "NumberedList" => ListType::Numbers,
            _ => ListType::None,
        });
        // A format with no counterpart (CJK, Arabic, Hebrew numbering) is left unset: decimal.
        a.number_style = e.prop("NumberingFormat").and_then(|v| names::numbering_format_in(&v));
        a.number_expression = e.prop("NumberingExpression");
        a.list_separator = e.prop("BulletsTextAfter");
        if let Some(b) = e.prop_el("BulletChar") {
            // A glyph of a named font (`GlyphWithFont`) has no character to map to.
            let unicode = matches!(b.get("BulletCharacterType"), Some("UnicodeOnly" | "UnicodeWithFont"));
            if let Some(c) = b.get("BulletCharacterValue").and_then(|v| v.trim().parse::<u32>().ok()).and_then(char::from_u32).filter(|_| unicode) {
                a.bullet_char = Some(c.to_string());
            }
        }
        a.balance_ragged = e.prop("BalanceRaggedLines").map(|v| v.trim() != "NoBalancing" && v.trim() != "false");
        a.shading_on = e.boolean("ParagraphShadingOn");
        if let Some(c) = e.prop("ParagraphShadingColor") {
            a.shading_color = Some(self.swatch_ref(c.trim()));
        }
        a.shading_tint = tint(e.num("ParagraphShadingTint"));
        let sides = |f: &dyn Fn(&str) -> String| -> Option<[f64; 4]> {
            let v = ["Top", "Left", "Bottom", "Right"].map(|s| e.num(&f(s)));
            v.iter().any(Option::is_some).then(|| v.map(|x| x.unwrap_or(0.0)))
        };
        a.shading_offsets = sides(&|s| format!("ParagraphShading{s}Offset"));
        a.border_on = e.boolean("ParagraphBorderOn");
        if let Some(c) = e.prop("ParagraphBorderColor") {
            a.border_color = Some(self.swatch_ref(c.trim()));
        }
        a.border_tint = tint(e.num("ParagraphBorderTint"));
        a.border_weights = sides(&|s| format!("ParagraphBorder{s}LineWeight"));
        a.border_offsets = sides(&|s| format!("ParagraphBorder{s}Offset"));
        a.border_type = e.prop("ParagraphBorderType").map(|v| names::stroke_type_in(v.trim()));
        a.border_cap = e.prop("ParagraphBorderStrokeEndCap").map(|v| names::cap_in(v.trim()));
        a.border_join = e.prop("ParagraphBorderStrokeEndJoin").map(|v| names::join_in(v.trim()));
        a.border_corners = corners(e, "ParagraphBorder");
        a.border_width = e.prop("ParagraphBorderWidth").map(|v| names::box_width_in(v.trim()));
        a.border_top = e.prop("ParagraphBorderTopOrigin").map(|v| names::box_top_in(v.trim()));
        a.border_bottom = e.prop("ParagraphBorderBottomOrigin").map(|v| names::box_bottom_in(v.trim()));
        a.border_display_if_splits = e.boolean("ParagraphBorderDisplayIfSplits");
        a.border_merge = e.boolean("MergeConsecutiveParaBorders");
        if let Some(c) = e.prop("ParagraphBorderGapColor") {
            a.border_gap_color = Some(self.swatch_ref(c.trim()));
        }
        a.border_gap_tint = tint(e.num("ParagraphBorderGapTint"));
        a.shading_corners = corners(e, "ParagraphShading");
        a.shading_width = e.prop("ParagraphShadingWidth").map(|v| names::box_width_in(v.trim()));
        a.shading_top = e.prop("ParagraphShadingTopOrigin").map(|v| names::box_top_in(v.trim()));
        a.shading_bottom = e.prop("ParagraphShadingBottomOrigin").map(|v| names::box_bottom_in(v.trim()));
        a.shading_clip = e.boolean("ParagraphShadingClipToFrame");
        a.shading_nonprinting = e.boolean("ParagraphShadingSuppressPrinting");
        a.span_space_before = e.num("SpanColumnMinSpaceBefore").filter(|v| v.is_finite());
        a.span_space_after = e.num("SpanColumnMinSpaceAfter").filter(|v| v.is_finite());
        a.list_level = e.num("NumberingLevel").filter(|v| v.is_finite()).map(|v| v.round().clamp(1.0, designcraft_doc::LIST_LEVELS as f64) as u32);
        if let Some(c) = e.prop("BulletsCharacterStyle") {
            a.bullet_char_style = Some(self.char_style_ref(c.trim()));
        }
        if let Some(c) = e.prop("NumberingCharacterStyle") {
            a.number_char_style = Some(self.char_style_ref(c.trim()));
        }
        a.bullet_align = e.prop("BulletsAlignment").map(|v| names::list_align_in(v.trim()));
        a.number_align = e.prop("NumberingAlignment").map(|v| names::list_align_in(v.trim()));
        a.restart_numbers = e.boolean("NumberingApplyRestartPolicy");
        if let Some(r) = e.prop_el("NumberingRestartPolicies") {
            // A specific level restarts after that level; any other policy after any higher one.
            let level = r.get("LowerLevel").and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(0);
            a.restart_after_level = Some(if r.get("RestartPolicy").map(str::trim) == Some("SpecificPreviousLevel") { level } else { 0 });
        }
        if let Some(f) = e.prop("BulletsFont") {
            let f = f.trim();
            a.bullet_font = Some(if f == "$ID/" { String::new() } else { f.split('\t').next().unwrap_or(f).to_string() });
        }
        if let Some(f) = e.prop("BulletsFontStyle") {
            let f = f.trim();
            a.bullet_font_style = Some(if f == "Nothing" || f == "$ID/" { String::new() } else { f.trim_start_matches("$ID/").to_string() });
        }
        a
    }

    fn rule(&mut self, e: &El, k: &str) -> Option<Rule> {
        // A style or range can set some of a rule's settings (such as only its colour); the others
        // come from its paragraph style chain, so the rule is whole.
        let fields = ["", "LineWeight", "Color", "Tint", "Width", "Offset", "LeftIndent", "RightIndent"];
        if !fields.iter().any(|f| e.prop(&format!("{k}{f}")).is_some()) {
            return None;
        }
        let v: Vec<Option<String>> = fields.iter().map(|f| self.para_setting(e, &format!("{k}{f}"))).collect();
        let get = |i: usize| v.get(i).cloned().flatten();
        let num = |i: usize| get(i).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite());
        let on = get(0).map(|v| v.trim() == "true");
        let weight = num(1);
        on.or(weight.map(|_| false))?;
        let mut r = Rule { on: on.unwrap_or(false), ..Rule::default() };
        if let Some(w) = weight {
            r.weight = w;
        }
        if let Some(t) = tint(num(3)) {
            r.tint = t;
        }
        r.column_width = get(4).is_none_or(|w| w.trim() != "TextWidth");
        r.offset = num(5).unwrap_or(0.0);
        r.left_indent = num(6).unwrap_or(0.0);
        r.right_indent = num(7).unwrap_or(0.0);
        if let Some(c) = get(2) {
            let c = c.trim();
            if c == designcraft_doc::TEXT_COLOR {
                r.color = c.to_string();
            } else if c.contains('/') {
                r.color = self.swatch_ref(c);
            }
        }
        Some(r)
    }

    /// Paragraph setting `k` of `e` (a paragraph style or a range): its own, else from its
    /// paragraph style chain (`BasedOn` of a style, the applied style of a range).
    fn para_setting(&self, e: &El, k: &str) -> Option<String> {
        if let Some(v) = e.prop(k) {
            return Some(v);
        }
        let style_ref = |r: String| if r.starts_with("ParagraphStyle/") { r } else { format!("ParagraphStyle/{}", names::escape_id(&r)) };
        let mut next =
            if e.local() == "ParagraphStyle" { e.prop("BasedOn") } else { e.get("AppliedParagraphStyle").map(str::to_string) }.map(style_ref);
        for _ in 0..16 {
            let Some(id) = next else { break };
            let s = self.para_els.get(&id)?;
            if let Some(v) = s.prop(k) {
                return Some(v);
            }
            next = s.prop("BasedOn").map(style_ref);
        }
        None
    }

    fn cell_style_ref(&self, reference: &str) -> String {
        let reference = reference.trim();
        self.cell_names
            .get(reference)
            .cloned()
            .unwrap_or_else(|| names::style_name_in(names::CELL_BUILTINS, &unescape_id(reference.trim_start_matches("CellStyle/"))))
    }

    fn table_style_ref(&self, reference: &str) -> String {
        let reference = reference.trim();
        self.table_names
            .get(reference)
            .cloned()
            .unwrap_or_else(|| names::style_name_in(names::TABLE_BUILTINS, &unescape_id(reference.trim_start_matches("TableStyle/"))))
    }

    fn cell_stroke_attrs(&mut self, el: &El, prefix: &str) -> designcraft_doc::CellStrokeAttrs {
        designcraft_doc::CellStrokeAttrs {
            weight: el.num(&format!("{prefix}StrokeWeight")).map(|v| v.max(0.0)),
            color: el.prop(&format!("{prefix}StrokeColor")).map(|v| self.swatch_ref(&v)),
            tint: tint(el.num(&format!("{prefix}StrokeTint"))),
            kind: el.prop(&format!("{prefix}StrokeType")).map(|v| names::stroke_type_in(&v)),
            gap_color: el.prop(&format!("{prefix}StrokeGapColor")).map(|v| self.swatch_ref(&v)),
            gap_tint: tint(el.num(&format!("{prefix}StrokeGapTint"))),
        }
    }

    fn alt_fill_attrs(&mut self, el: &El, kind: &str) -> designcraft_doc::AltFillsAttrs {
        designcraft_doc::AltFillsAttrs {
            first: el.num(&format!("Start{kind}FillCount")).map(|v| v.max(0.0) as u32),
            first_color: el.prop(&format!("Start{kind}FillColor")).map(|v| self.swatch_ref(&v)),
            first_tint: tint(el.num(&format!("Start{kind}FillTint"))),
            next: el.num(&format!("End{kind}FillCount")).map(|v| v.max(0.0) as u32),
            next_color: el.prop(&format!("End{kind}FillColor")).map(|v| self.swatch_ref(&v)),
            next_tint: tint(el.num(&format!("End{kind}FillTint"))),
            skip_first: el.num(&format!("SkipFirstAlternatingFill{kind}s")).map(|v| v.max(0.0) as u32),
            skip_last: el.num(&format!("SkipLastAlternatingFill{kind}s")).map(|v| v.max(0.0) as u32),
        }
    }

    fn alt_stroke_attrs(&mut self, el: &El, kind: &str) -> designcraft_doc::AltStrokesAttrs {
        let first_stroke = self.cell_stroke_attrs(el, &format!("Start{kind}"));
        let mut next_stroke = self.cell_stroke_attrs(el, &format!("End{kind}"));
        // IDML uses LineStyle for this one attribute (rather than StrokeType).
        if kind == "Column"
            && let Some(value) = el.prop("EndColumnLineStyle")
        {
            next_stroke.kind = Some(names::stroke_type_in(&value));
        }
        designcraft_doc::AltStrokesAttrs {
            first: el.num(&format!("Start{kind}StrokeCount")).map(|v| v.max(0.0) as u32),
            next: el.num(&format!("End{kind}StrokeCount")).map(|v| v.max(0.0) as u32),
            first_stroke,
            next_stroke,
            skip_first: el.num(&format!("SkipFirstAlternatingStroke{kind}s")).map(|v| v.max(0.0) as u32),
            skip_last: el.num(&format!("SkipLastAlternatingStroke{kind}s")).map(|v| v.max(0.0) as u32),
        }
    }

    // ---------- stories ----------

    fn story(&mut self, id: StoryId, e: &El) -> Story {
        self.story_with_para(id, e, ParaFormat::default())
    }

    fn story_with_para(&mut self, id: StoryId, e: &El, default_para: ParaFormat) -> Story {
        let mut b = StoryBuilder {
            text: String::new(),
            paras: vec![default_para.clone()],
            runs: Vec::new(),
            fresh: true,
            last: CharFormat::default(),
            tables: Vec::new(),
            after_table: None,
            notes: Vec::new(),
            anchors: Vec::new(),
            xrefs: Vec::new(),
            index_refs: Vec::new(),
            objects: Vec::new(),
        };
        self.walk_story(e, &mut b, &default_para, &CharAttrs::default(), &CharFormat::default(), None);
        let StoryBuilder { text, mut paras, runs, last, tables: tbls, notes, anchors, xrefs, index_refs, objects, .. } = b;
        let mut chars: Vec<CharRun> = runs.into_iter().filter(|r| r.len > 0).collect();
        if chars.is_empty() {
            chars.push(CharRun { len: 0, format: last });
        }
        let mut tables = BTreeMap::new();
        for (pi, t) in tbls {
            if let Some(p) = paras.get_mut(pi) {
                p.table = Some(t.id);
                tables.insert(t.id, Arc::new(t));
            }
        }
        let notes = notes.into_iter().enumerate().map(|(i, text)| Arc::new(designcraft_doc::Footnote { id: i as u64 + 1, text })).collect();
        let mut anchor_list = Vec::new();
        for (k, (me, name)) in anchors.into_iter().enumerate() {
            let aid = (id.0 << 24) | (k as u64 + 1);
            self.anchor_ids.insert(me, aid);
            anchor_list.push(Arc::new(designcraft_doc::TextAnchor { id: aid, name }));
        }
        let mut xref_list = Vec::new();
        for (k, (me, fmt)) in xrefs.into_iter().enumerate() {
            self.xref_srcs.insert(me, (id, k));
            let format = self.xref_format_names.get(&fmt).cloned().unwrap_or_else(|| "Full Paragraph & Page Number".into());
            xref_list.push(Arc::new(designcraft_doc::CrossRef { target: 0, format }));
        }
        let pref = e.find("StoryPreference");
        let optical_margin = designcraft_doc::OpticalMargin {
            on: pref.and_then(|p| p.boolean("OpticalMarginAlignment")).unwrap_or(false),
            size: pref
                .and_then(|p| p.num("OpticalMarginSize"))
                .and_then(designcraft_doc::OpticalMargin::clamp_size)
                .unwrap_or(designcraft_doc::OpticalMargin::DEFAULT_SIZE),
        };
        let mut st = Story {
            id,
            text,
            paras,
            chars,
            frames: vec![],
            direction: if e.find("StoryPreference").and_then(|p| p.get("StoryDirection")) == Some("RightToLeftDirection") {
                designcraft_doc::TextDirection::RightToLeft
            } else {
                designcraft_doc::TextDirection::LeftToRight
            },
            vertical: e.find("StoryPreference").and_then(|p| p.get("StoryOrientation")) == Some("Vertical"),
            optical_margin,
            rev: 0,
            tables,
            notes,
            endnotes: Vec::new(),
            anchors: anchor_list,
            xrefs: xref_list,
            index_refs: index_refs.into_iter().map(Arc::new).collect(),
            objects: objects.into_iter().map(Arc::new).collect(),
            editorial: Vec::new(),
            link: None,
            file_link: None,
        };
        st.fix_notes();
        st.fix_marks();
        // Drop anything inconsistent (e.g. a table sharing a paragraph with another).
        if st.check().is_err() {
            st.tables.retain(|_, _| false);
            for p in &mut st.paras {
                p.table = None;
            }
        }
        st
    }

    /// `<Table>`: rows, columns and cells (cell `Name` is `column:row`).
    fn table(&mut self, e: &El) -> Table {
        let id = self.alloc();
        let rows: Vec<&El> = e.find_all("Row").collect();
        let cols: Vec<&El> = e.find_all("Column").collect();
        let nr = rows.len().max(1);
        let nc = cols.len().max(1);
        let mut t = Table::new(id, nr, nc, 0, 0, 0.0);
        t.options.direction = if e.get("TableDirection") == Some("RightToLeftDirection") {
            designcraft_doc::TextDirection::RightToLeft
        } else {
            designcraft_doc::TextDirection::LeftToRight
        };
        for (r, re) in rows.iter().enumerate() {
            let auto = re.boolean("AutoGrow").unwrap_or(true);
            t.rows[r].mode = if auto { RowHeightMode::AtLeast } else { RowHeightMode::Exactly };
            t.rows[r].height = if auto { re.num("MinimumHeight").unwrap_or(3.0) } else { re.num("SingleRowHeight").unwrap_or(12.0) };
        }
        for (c, ce) in cols.iter().enumerate() {
            t.columns[c].width = ce.num("SingleColumnWidth").unwrap_or(72.0).max(3.0);
        }
        let h = e.num("HeaderRowCount").unwrap_or(0.0) as usize;
        let f = e.num("FooterRowCount").unwrap_or(0.0) as usize;
        if h.saturating_add(f) < nr {
            t.set_header_footer(h, f);
        }
        let style_name = e.get("AppliedTableStyle").map(|r| self.table_style_ref(r)).unwrap_or_else(|| designcraft_doc::BASIC_TABLE.into());
        let style = self.styles.resolve_table_style(&style_name);
        style.apply_to(&mut t, &self.styles.cell);
        // IDML distinguishes an explicit AppliedCellStyle from the effective region style.
        for cell in &mut t.cells {
            cell.style.clear();
        }
        // Table-local options override the named table style. Cell styles are already
        // materialized independently, so their edges retain their higher precedence.
        let mut border_unset = style.border.is_none() && style.borders.iter().all(|b| b.is_empty());
        let mut borders = std::array::from_fn::<_, 4, _>(|i| t.options.border_for(i).clone());
        for (side, border) in ["TopBorder", "LeftBorder", "BottomBorder", "RightBorder"].iter().zip(&mut borders) {
            let attrs = self.cell_stroke_attrs(e, side);
            border_unset &= attrs.is_empty();
            attrs.apply_to(border);
        }
        t.options.border = borders[0].clone();
        t.options.borders = borders.map(Some);
        t.options.space_before = e.num("SpaceBefore").unwrap_or(t.options.space_before);
        t.options.space_after = e.num("SpaceAfter").unwrap_or(t.options.space_after);
        t.options.repeat_header = e.get("HeaderBehavior") != Some("RepeatOnce");
        t.options.repeat_footer = e.get("FooterBehavior") != Some("RepeatOnce");
        // A local count may enable a pattern disabled by its parent style. Merge before
        // materialization so the parent's colors/tints are still available in that case.
        let mut row_fills = style.row_fills.clone();
        row_fills.merge(&self.alt_fill_attrs(e, "Row"));
        row_fills.apply_to(&mut t.options.alt_rows);
        let mut column_fills = style.column_fills.clone();
        column_fills.merge(&self.alt_fill_attrs(e, "Column"));
        column_fills.apply_to(&mut t.options.alt_cols);
        let mut row_strokes = style.row_strokes.clone();
        row_strokes.merge(&self.alt_stroke_attrs(e, "Row"));
        row_strokes.apply_to(&mut t.options.row_strokes);
        let mut column_strokes = style.column_strokes.clone();
        column_strokes.merge(&self.alt_stroke_attrs(e, "Column"));
        column_strokes.apply_to(&mut t.options.column_strokes);
        let mut regions = Vec::new();
        for ce in e.find_all("Cell") {
            let Some((c, r)) =
                ce.get("Name").and_then(|n| n.split_once(':')).and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
            else {
                continue;
            };
            if r >= nr || c >= nc {
                continue;
            }
            let rs = (span(ce, "RowSpan").max(1.0) as usize).min(nr - r);
            let cs = (span(ce, "ColumnSpan").max(1.0) as usize).min(nc - c);
            let mut cell = t.cell(r, c).cloned().unwrap_or_default();
            let cell_style = ce.get("AppliedCellStyle").map(|r| self.cell_style_ref(r)).unwrap_or_else(|| designcraft_doc::NO_CELL_STYLE.into());
            if cell_style != designcraft_doc::NO_CELL_STYLE {
                designcraft_doc::resolve_cell_style(&self.styles.cell, &cell_style).apply_to(&mut cell);
            }
            cell.style = if cell_style == designcraft_doc::NO_CELL_STYLE { String::new() } else { cell_style };
            let default_para = cell.text.paras.first().cloned().unwrap_or_default();
            cell.text = self.story_with_para(StoryId(0), ce, default_para);
            if let Some(fc) = ce.get("FillColor") {
                cell.fill = self.swatch_ref(fc);
            }
            if let Some(tint) = tint(ce.num("FillTint")) {
                cell.fill_tint = tint;
            }
            for (i, k) in ["TopInset", "LeftInset", "BottomInset", "RightInset"].iter().enumerate() {
                if let Some(v) = ce.num(k) {
                    cell.insets[i] = v.max(0.0);
                }
            }
            if let Some(vj) = ce.get("VerticalJustification") {
                cell.vj = names::vj_in(vj);
            }
            if let Some(fb) = ce.get("FirstBaselineOffset") {
                cell.first_baseline = names::first_baseline_in(fb);
            }
            if let Some(min) = ce.num("MinimumFirstBaselineOffset").filter(|v| v.is_finite()) {
                cell.first_baseline_min = min.clamp(0.0, 8640.0);
            }
            cell.rotation = ce.num("RotationAngle").unwrap_or(cell.rotation);
            if cell.rotation != 0.0 {
                self.rotated_cells += 1;
            }
            for (i, side) in ["TopEdge", "LeftEdge", "BottomEdge", "RightEdge"].iter().enumerate() {
                let attrs = self.cell_stroke_attrs(ce, side);
                attrs.apply_to(&mut cell.strokes[i]);
                cell.stroke_defined[i] |= !attrs.is_empty();
                let priority = ce.prop(&format!("{side}StrokePriority")).and_then(|v| v.trim().parse::<i32>().ok()).unwrap_or(0);
                cell.stroke_priorities[i] = priority;
                // Preserve style-defined edges and #102's positive-priority local overrides.
                cell.border_overrides[i] |= priority > 0;
                // No table or table style border is given anywhere, so the built-in 1 pt default must not
                // draw over an outer edge the cell explicitly hides (weight 0 or the None swatch), as InDesign draws nothing.
                let hidden = attrs.weight == Some(0.0) || attrs.color.as_deref() == Some(swatch::NONE);
                cell.border_overrides[i] |= border_unset && hidden;
            }
            if let Some(slot) = t.cell_mut(r, c) {
                *slot = cell;
            }
            if rs > 1 || cs > 1 {
                regions.push(CellRange { r0: r, c0: c, r1: (r + rs - 1).min(nr - 1), c1: (c + cs - 1).min(nc - 1) });
            }
        }
        for rg in regions {
            let _ = t.merge(rg);
        }
        t
    }

    /// The format of the text in a `CharacterStyleRange`: its style, and its local attributes over
    /// the enclosing paragraph range's, inside a tracked change if `outer` (the enclosing format) is.
    fn range_format(&mut self, c: &El, pchars: &CharAttrs, outer: &CharFormat) -> CharFormat {
        let style = c.get("AppliedCharacterStyle").map(|r| self.char_style_ref(r)).unwrap_or_else(|| st::NO_CHAR_STYLE.into());
        let mut over = pchars.clone();
        over.merge(&self.char_attrs(c));
        over.change = over.change.or(outer.over.change);
        CharFormat { style, over }
    }

    fn walk_story(&mut self, e: &El, b: &mut StoryBuilder, pf: &ParaFormat, pchars: &CharAttrs, cf: &CharFormat, brk: Option<&str>) {
        for n in &e.children {
            match n {
                Node::El(c) => match c.local() {
                    "ParagraphStyleRange" => {
                        let style = c.get("AppliedParagraphStyle").map(|r| self.para_style_ref(r)).unwrap_or_else(|| pf.style.clone());
                        let para = self.para_attrs(c);
                        let mut chars = self.char_attrs(c);
                        // A paragraph range inside a tracked change is part of that change.
                        chars.change = chars.change.or(cf.over.change);
                        let npf = ParaFormat { style, para, chars: CharAttrs::default(), table: None };
                        if b.fresh
                            && let Some(last) = b.paras.last_mut()
                        {
                            *last = npf.clone();
                        }
                        // Text and markers outside any character range (a destination or a
                        // cross-reference source at paragraph level) take the paragraph's format.
                        let pcf = CharFormat { style: st::NO_CHAR_STYLE.into(), over: chars.clone() };
                        self.walk_story(c, b, &npf, &chars, &pcf, None);
                    }
                    "CharacterStyleRange" => {
                        let ncf = self.range_format(c, pchars, cf);
                        let bt = c.get("ParagraphBreakType").filter(|t| *t != "Anywhere");
                        b.last = ncf.clone();
                        self.walk_story(c, b, pf, pchars, &ncf, bt);
                    }
                    "Content" => {
                        for t in &c.children {
                            match t {
                                Node::Text(s) | Node::CData(s) => {
                                    let s: String = s
                                        .chars()
                                        .filter(|c| *c != '\u{FEFF}')
                                        .map(|c| if c == '\n' || c == '\r' || c == '\u{2029}' { st::FORCED_LINE_BREAK } else { c })
                                        .collect();
                                    b.push(&s, cf);
                                }
                                Node::Pi(t, v) if t == "ACE" => {
                                    if let Some(ch) = ace_char(v) {
                                        b.push(&ch.to_string(), cf);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "Br" => {
                        // A column, frame or page break ends its paragraph like a return does.
                        if let Some(t) = brk {
                            let ch = match t {
                                "NextColumn" => st::COLUMN_BREAK,
                                "NextFrame" => st::FRAME_BREAK,
                                "NextOddPage" => st::ODD_PAGE_BREAK,
                                "NextEvenPage" => st::EVEN_PAGE_BREAK,
                                _ => st::PAGE_BREAK,
                            };
                            b.push(&ch.to_string(), cf);
                        }
                        b.push("\n", cf);
                        b.paras.push(pf.clone());
                        b.fresh = true;
                    }
                    "Table" => {
                        let t = self.table(c);
                        b.push_table(t, pf, cf);
                    }
                    "HyperlinkTextDestination" | "ParagraphDestination" => {
                        b.anchors.push((c.get("Self").unwrap_or("").to_string(), c.get("Name").unwrap_or("Anchor").to_string()));
                        b.push(&designcraft_doc::ANCHOR_MARK.to_string(), cf);
                    }
                    "PageReference" => {
                        use designcraft_doc::index::IndexRange as R;
                        let topic = c.get("ReferencedTopic").and_then(|r| self.index_topics.get(r)).cloned().unwrap_or_default();
                        let range = match c.get("PageReferenceType") {
                            Some("ToEndOfStory") | Some("ToEndOfDocument") | Some("ToEndOfSection") => R::ToEndOfStory,
                            Some("ForNextNParagraphs") => R::NextParagraphs(c.num("PageReferenceLimit").unwrap_or(1.0).max(1.0) as u32),
                            Some("SuppressPageNumbers") => R::SuppressPageRange,
                            _ => R::CurrentPage,
                        };
                        b.index_refs.push(designcraft_doc::IndexRef { topics: topic.path, sort: topic.sort, range });
                        b.push(&designcraft_doc::INDEX_MARK.to_string(), cf);
                    }
                    "CrossReferenceSource" => {
                        // The generated text is regenerated from the format.
                        b.xrefs.push((c.get("Self").unwrap_or("").to_string(), c.get("AppliedFormat").unwrap_or("").to_string()));
                        // A source at paragraph level wraps a range; inside a range it holds the
                        // text directly and takes that range's format.
                        let f = c.find("CharacterStyleRange").map_or_else(|| cf.clone(), |r| self.range_format(r, pchars, cf));
                        b.push(&designcraft_doc::XREF_MARK.to_string(), &f);
                    }
                    "Footnote" => {
                        // The text starts with the number marker (ACE 4, dropped) and the separator.
                        let mut note = self.story(StoryId(0), c);
                        let sep = self.footnote_options.separator.clone();
                        if !sep.is_empty() && note.text.starts_with(&sep) {
                            note.delete(0..sep.len());
                        }
                        b.notes.push(note);
                        // The reference's position comes from the footnote options.
                        let mut rcf = cf.clone();
                        rcf.over.position = None;
                        b.push(&designcraft_doc::FOOTNOTE_REF.to_string(), &rcf);
                    }
                    // Anchored page items, including groups containing text frames.
                    "Rectangle" | "Oval" | "Polygon" | "GraphicLine" | "Group" | "TextFrame" => {
                        if let Some(item) = self.item(c, designcraft_geom::Affine::IDENTITY) {
                            use designcraft_doc::anchored::AnchorAlign as A;
                            // Each setting the item leaves unset comes from its object style chain,
                            // then from the IDML defaults.
                            let levels: Vec<&El> = std::iter::once(c).chain(self.object_style_chain(c)).collect();
                            let s = |k: &str| {
                                let set = levels.iter().filter_map(|l| l.find("AnchoredObjectSetting")).find_map(|x| x.get(k));
                                set.unwrap_or(match k {
                                    "AnchoredPosition" => "InlinePosition",
                                    "AnchorPoint" => "BottomRightAnchor",
                                    "HorizontalAlignment" => "LeftAlign",
                                    "HorizontalReferencePoint" => "TextFrame",
                                    "VerticalAlignment" => "BottomAlign",
                                    "VerticalReferencePoint" => "LineBaseline",
                                    "PinPosition" => "true",
                                    _ => "",
                                })
                            };
                            let get = |k: &str| s(k).trim().parse::<f64>().ok().filter(|v| v.is_finite()).unwrap_or(0.0);
                            let position = match s("AnchoredPosition") {
                                "AboveLine" => designcraft_doc::AnchorPosition::AboveLine {
                                    align: match s("HorizontalAlignment") {
                                        "CenterAlign" => A::Center,
                                        "RightAlign" => A::Right,
                                        _ => A::Left,
                                    },
                                    space_before: get("AnchorSpaceAbove"),
                                    space_after: get("AnchorYoffset"),
                                },
                                "Anchored" => {
                                    let col = match s("HorizontalAlignment") {
                                        "CenterAlign" => 1,
                                        "RightAlign" => 2,
                                        _ => 0,
                                    };
                                    let row = match s("VerticalAlignment") {
                                        "CenterAlign" => 1,
                                        "BottomAlign" => 2,
                                        _ => 0,
                                    };
                                    designcraft_doc::AnchorPosition::Custom {
                                        x_relative: names::anchor_rel_in(s("HorizontalReferencePoint")),
                                        y_relative: names::anchor_rel_in(s("VerticalReferencePoint")),
                                        // IDML measures a left-aligned object's X offset leftwards.
                                        x_offset: if col == 0 { 0.0 - get("AnchorXoffset") } else { get("AnchorXoffset") },
                                        y_offset: get("AnchorYoffset"),
                                        object_point: names::anchor_in(s("AnchorPoint")),
                                        ref_point: row * 3 + col,
                                        keep_within_column: s("PinPosition") == "true",
                                        spine_relative: s("SpineRelative") == "true",
                                    }
                                }
                                _ => designcraft_doc::AnchorPosition::Inline { y_offset: get("AnchorYoffset") },
                            };
                            b.objects.push(designcraft_doc::AnchoredObject::new(item, position));
                            b.push(&designcraft_doc::OBJECT_MARK.to_string(), cf);
                        }
                    }
                    "TextVariableInstance" => {
                        let t = self.text_variable_instance(c);
                        b.push(&t, cf);
                    }
                    "Change" => {
                        // Tracked changes: deleted text stays in the story, marked, and takes no
                        // room in the layout.
                        let mut ncf = cf.clone();
                        match c.get("ChangeType") {
                            Some("DeletedText") => ncf.over.change = Some(designcraft_doc::ChangeMark::Deleted),
                            Some("InsertedText" | "MovedText") => ncf.over.change = Some(designcraft_doc::ChangeMark::Inserted),
                            _ => {}
                        }
                        self.walk_story(c, b, pf, pchars, &ncf, brk);
                    }
                    "Properties" | "Note" | "StoryPreference" | "InCopyExportOption" => {}
                    _ => self.walk_story(c, b, pf, pchars, cf, brk),
                },
                Node::Pi(t, v) if t == "ACE" => {
                    if let Some(ch) = ace_char(v) {
                        b.push(&ch.to_string(), cf);
                    }
                }
                _ => {}
            }
        }
    }

    // ---------- spreads and items ----------

    /// Ruler guides of a `<Page>` (its `<Guide>` children), `page` its rectangle in our spread
    /// coordinates. A guide's Location is measured from the ruler origin: the spread's top left, the
    /// page's, or the spine. Fit to Page off: the guide crosses the spread.
    fn guides_of(&self, p: &El, page: Rect, spine: f64) -> Vec<Guide> {
        p.find_all("Guide")
            .filter_map(|g| {
                let at = g.num("Location").filter(|v| v.is_finite() && v.abs() < 1e7)?;
                let orientation = if g.get("Orientation") == Some("Horizontal") { Orientation::Horizontal } else { Orientation::Vertical };
                let position = match (orientation, self.settings.ruler_origin) {
                    (Orientation::Horizontal, designcraft_doc::RulerOrigin::Page) => page.y0 + at,
                    (Orientation::Horizontal, _) => at,
                    (Orientation::Vertical, designcraft_doc::RulerOrigin::Page) => page.x0 + at,
                    (Orientation::Vertical, designcraft_doc::RulerOrigin::Spine) => spine + at,
                    (Orientation::Vertical, designcraft_doc::RulerOrigin::Spread) => at,
                };
                Some(Guide {
                    orientation,
                    position,
                    spread: g.get("FitToPage") == Some("false"),
                    locked: g.get("Locked") == Some("true"),
                    layer: g.get("ItemLayer").and_then(|l| self.layer_ids.get(l)).copied(),
                    liquid: g.get("GuideType") == Some("Liquid"),
                })
            })
            .collect()
    }

    fn spread(&mut self, e: &El, id: SpreadId, master: bool) -> Spread {
        let facing = self.settings.facing_pages;
        // Page rectangles in IDML spread coordinates.
        let mut pages: Vec<(Rect, &El)> = Vec::new();
        for p in e.find_all("Page") {
            let gb = nums(p.get("GeometricBounds").unwrap_or(""));
            let (t, l, bt, r) =
                if gb.len() == 4 { (gb[0], gb[1], gb[2], gb[3]) } else { (0.0, 0.0, self.settings.page_height, self.settings.page_width) };
            let xf = parse_affine(p.get("ItemTransform"));
            let a = xf * Point::new(l, t);
            let c = xf * Point::new(r, bt);
            pages.push((Rect::from_points(a, c), p));
        }
        let min_x = pages.iter().map(|(r, _)| r.x0).fold(f64::INFINITY, f64::min);
        let min_y = pages.iter().map(|(r, _)| r.y0).fold(f64::INFINITY, f64::min);
        let (min_x, min_y) = if min_x.is_finite() { (min_x, min_y) } else { (0.0, 0.0) };
        let shift = Affine::translate((-min_x, -min_y));
        // The spine: between a left and a right page (else the spread's left edge).
        let spine = pages.iter().map(|(r, _)| r.x0 - min_x).filter(|x| *x > 1e-6).fold(f64::INFINITY, f64::min);
        let spine = if spine.is_finite() && facing { spine } else { 0.0 };
        let show = e.get("ShowMasterItems") != Some("false");
        let first_abs: usize = self.spreads.iter().map(|s| s.pages.len()).sum();
        let view_rotation = if master { 0 } else { spread_view_rotation(e.get("ItemTransform")) };
        let mut out_pages = Vec::new();
        for (i, (r, p)) in pages.iter().enumerate() {
            let pid = PageId(self.alloc());
            if let Some(s) = p.get("Self")
                && !master
            {
                self.page_index.insert(s.to_string(), first_abs + i);
            }
            let side = if !facing {
                PageSide::Single
            } else if r.x1 <= 1e-6 {
                PageSide::Left
            } else {
                PageSide::Right
            };
            let parent = p.get("AppliedMaster").and_then(|m| self.parent_ids.get(m)).copied();
            // A document page's margins are local when they differ from its parent page's; a
            // page without its own takes the parent page's.
            let from_parent = if master {
                None
            } else {
                parent
                    .and_then(|id| self.parents.iter().find(|sp| sp.id == id))
                    .and_then(|sp| sp.pages.get(sp.parent_page_for_side(side)?))
                    .map(|q| (q.margins, q.columns.clone()))
            };
            let (margins, columns) = match (p.find("MarginPreference"), &from_parent) {
                (Some(m), _) => {
                    let (margins, columns) = margins_of(m);
                    (margins, fit_columns(columns, &margins, r.width()))
                }
                (None, Some(own)) => own.clone(),
                (None, None) => {
                    let (margins, columns) = self.default_margins.clone().unwrap_or((Margins::uniform(36.0), Columns::default()));
                    (margins, fit_columns(columns, &margins, r.width()))
                }
            };
            let local_margins = from_parent.is_some_and(|(m, c)| m != margins || c != columns);
            out_pages.push(Page {
                id: pid,
                width: r.width(),
                height: r.height(),
                x: r.x0 - min_x,
                margins,
                columns,
                local_margins,
                parent,
                side,
                overridden: vec![],
                guides: self.guides_of(p, Rect::new(r.x0 - min_x, r.y0 - min_y, r.x1 - min_x, r.y1 - min_y), spine),
                show_parent_items: show,
                liquid: names::liquid_rule_in(p.get("LayoutRule").unwrap_or("Off")),
                transition: None,
                view_rotation: if i == 0 { view_rotation } else { 0 },
                show_parent_overlay: false,
            });
        }
        let overrides: Vec<Vec<String>> =
            pages.iter().map(|(_, p)| p.get("OverrideList").unwrap_or("").split_whitespace().map(str::to_string).collect()).collect();
        let mut items = Vec::new();
        for c in e.elements() {
            if let Some(it) = self.item(c, shift) {
                items.push(Arc::new(it));
            }
        }
        for (pg, ov) in out_pages.iter_mut().zip(overrides) {
            pg.overridden = ov.iter().filter_map(|s| self.item_ids.get(s)).copied().filter(|i| self.is_parent_item(*i)).collect();
        }
        let parent = if master {
            let prefix =
                e.get("NamePrefix").map(str::to_string).unwrap_or_else(|| e.get("Name").unwrap_or("A").split('-').next().unwrap_or("A").to_string());
            let name = e.get("BaseName").map(str::to_string).unwrap_or_else(|| {
                let n = e.get("Name").unwrap_or("Parent");
                n.split_once('-').map(|(_, b)| b.to_string()).unwrap_or_else(|| n.to_string())
            });
            // A parent spread has one base: its first page's. Pages that disagree lose theirs.
            let applied: Vec<Option<SpreadId>> =
                pages.iter().map(|(_, p)| p.get("AppliedMaster").and_then(|m| self.parent_ids.get(m)).copied()).collect();
            let label = format!("{prefix}-{name}");
            if applied.windows(2).any(|w| w[0] != w[1]) {
                self.warnings.push(format!("parent {label}: its pages are based on different parents; only the first page's is kept"));
            }
            let based_on = applied.first().copied().flatten();
            if applied.contains(&Some(id)) {
                self.warnings.push(format!("parent {label}: a page names its own parent as its base, which is ignored"));
            }
            let based_on = based_on.filter(|b| *b != id);
            Some(ParentInfo { prefix, name, based_on })
        } else {
            None
        };
        for p in &mut out_pages {
            if master {
                p.parent = None;
            }
        }
        Spread { id, pages: out_pages, items, parent, allow_shuffle: e.get("AllowPageShuffle") != Some("false") }
    }

    fn is_parent_item(&self, id: ItemId) -> bool {
        self.parents.iter().any(|p| {
            p.items.iter().any(|it| {
                let mut found = false;
                it.walk(&mut |i| found |= i.id == id);
                found
            })
        })
    }

    /// An item's object style chain: its applied style, the styles that one is based on, then
    /// `[None]` (cycle-safe).
    fn object_style_chain(&self, e: &El) -> Vec<&El> {
        let mut chain = self.object_style_bases(e.get("AppliedObjectStyle").map(str::to_string));
        if let Some(none) = self.object_els.get("ObjectStyle/$ID/[None]")
            && !chain.iter().any(|c| std::ptr::eq(*c, none))
        {
            chain.push(none);
        }
        chain
    }

    /// Object style `id` and the styles it is based on, nearest first (cycle-safe, at most 16).
    fn object_style_bases(&self, id: Option<String>) -> Vec<&El> {
        let mut chain: Vec<&El> = Vec::new();
        let mut os = id;
        while let Some(id) = os {
            let Some(s) = self.object_els.get(&id) else { break };
            if chain.len() >= 16 || chain.iter().any(|c| std::ptr::eq(*c, s)) {
                break;
            }
            chain.push(s);
            os = s.prop("BasedOn").map(|b| {
                let b = b.trim();
                if b.starts_with("ObjectStyle/") { b.to_string() } else { format!("ObjectStyle/{}", names::escape_id(b)) }
            });
        }
        chain
    }

    /// Resolve an item attribute, falling back to its object style chain.
    fn attr_or_style(&self, e: &El, k: &str) -> Option<String> {
        e.get(k).or_else(|| self.object_style_chain(e).into_iter().find_map(|s| s.get(k))).map(str::to_string)
    }

    /// The item's child element `name` laid over the same element of each style in its object style
    /// chain: attributes and nested elements merge one by one, nearer levels winning. IDML writes on an
    /// item only the values that differ from its style. Effects whose category the applied style turns
    /// off (`ObjectStyle*EffectsCategorySettings`) come from the item alone.
    fn with_style(&self, e: &El, name: &str) -> Option<El> {
        let chain = self.object_style_chain(e);
        let category = match name {
            "TransparencySetting" => Some("ObjectStyleObjectEffectsCategorySettings"),
            "StrokeTransparencySetting" => Some("ObjectStyleStrokeEffectsCategorySettings"),
            "FillTransparencySetting" => Some("ObjectStyleFillEffectsCategorySettings"),
            "ContentTransparencySetting" => Some("ObjectStyleContentEffectsCategorySettings"),
            _ => None,
        };
        let enabled = |effect: &str| {
            let Some(flag) = category.and_then(|_| effect_flag(effect)) else { return true };
            chain.iter().find_map(|s| category.and_then(|c| s.find(c)).and_then(|c| c.get(flag))) != Some("false")
        };
        let mut out: Option<El> = None;
        for s in chain.iter().rev() {
            let Some(c) = s.find(name) else { continue };
            let mut c = c.clone();
            c.children.retain(|n| !matches!(n, Node::El(x) if !enabled(x.local())));
            match &mut out {
                Some(o) => overlay(o, &c, 0),
                None => out = Some(c),
            }
        }
        if let Some(c) = e.find(name) {
            match &mut out {
                Some(o) => overlay(o, c, 0),
                None => out = Some(c.clone()),
            }
        }
        out
    }

    /// Reads the drop shadow, feather, gradient feather and directional feather of a
    /// `TransparencySetting` into `fx`.
    fn effects_in(&mut self, t: &El, fx: &mut Effects) {
        if let Some(ds) = t.find("DropShadowSetting")
            && ds.get("Mode") == Some("Drop")
        {
            let x = ds.num("XOffset").unwrap_or(4.95);
            let y = ds.num("YOffset").unwrap_or(4.95);
            let def = DropShadow::default();
            fx.drop_shadow = DropShadow {
                on: true,
                color: ds.get("EffectColor").map(|c| self.swatch_ref(c)).unwrap_or(def.color),
                opacity: ds.num("Opacity").map(|o| (o / 100.0) as f32).unwrap_or(def.opacity),
                angle: names_angle(x, y),
                distance: x.hypot(y),
                size: ds.num("Size").unwrap_or(def.size),
                spread: ds.num("Spread").unwrap_or(def.spread),
                global_light: false,
            };
        }
        if let Some(f) = t.find("FeatherSetting")
            && f.get("Mode").is_some_and(|m| m != "None")
        {
            fx.feather = f.num("Width").unwrap_or(9.0);
            fx.feather_options.choke = finite(f.num("ChokeAmount")).unwrap_or(0.0).clamp(0.0, 100.0);
            fx.feather_options.corners = match f.get("CornerType") {
                Some("Sharp") => designcraft_doc::FeatherCorners::Sharp,
                Some("Rounded") => designcraft_doc::FeatherCorners::Rounded,
                _ => designcraft_doc::FeatherCorners::Diffused,
            };
        }
        if let Some(g) = t.find("GradientFeatherSetting")
            && g.boolean("Applied") == Some(true)
        {
            fx.gradient_feather = gradient_feather_in(g);
        }
        if let Some(df) = t.find("DirectionalFeatherSetting")
            && df.boolean("Applied") == Some(true)
        {
            let w = |k: &str| finite(df.num(k)).unwrap_or(0.0).max(0.0);
            fx.directional_feather = DirectionalFeather { on: true, widths: [w("TopWidth"), w("LeftWidth"), w("BottomWidth"), w("RightWidth")] };
        }
    }

    /// An object style's drop shadow and feather. Each is read when the effects category turns it
    /// on (without category settings: when its setting is on), from the nearest style in the
    /// BasedOn chain that has the setting. `None` when the style sets neither.
    fn style_effects(&mut self, e: &El) -> Option<Effects> {
        let mut chain = self.object_style_bases(e.get("Self").map(str::to_string));
        if chain.is_empty() {
            chain.push(e);
        }
        let category = chain.iter().find_map(|s| s.find("ObjectStyleObjectEffectsCategorySettings")).cloned();
        let setting = |name: &str| chain.iter().find_map(|s| s.find("TransparencySetting")?.find(name)).cloned();
        let mut t = El::new("TransparencySetting");
        let mut any = false;
        for (name, flag) in [("DropShadowSetting", "EnableDropShadow"), ("FeatherSetting", "EnableFeather")] {
            let el = setting(name);
            let on = match &category {
                Some(c) => c.get(flag) == Some("true"),
                None => el.as_ref().and_then(|x| x.get("Mode")).is_some_and(|m| m != "None"),
            };
            any |= on;
            if let (true, Some(el)) = (on, el) {
                t.push(el);
            }
        }
        if !any {
            return None;
        }
        let mut fx = Effects::default();
        self.effects_in(&t, &mut fx);
        Some(fx)
    }

    fn stroke_from(&mut self, e: &El, item: Option<&El>) -> Stroke {
        let g = |s: &Self, k: &str| -> Option<String> { if item.is_some() { s.attr_or_style(e, k) } else { e.get(k).map(str::to_string) } };
        let color = g(self, "StrokeColor");
        let mut s = Stroke::none();
        if let Some(c) = color {
            s.swatch = self.swatch_ref(&c);
        }
        if let Some(w) = g(self, "StrokeWeight").and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite()) {
            s.weight = w;
        }
        if let Some(t) = g(self, "StrokeTint").and_then(|v| v.parse().ok()) {
            s.tint = tint(Some(t)).unwrap_or(1.0);
        }
        if let Some(t) = g(self, "StrokeType") {
            s.kind = names::stroke_type_in(&t);
        }
        if let Some(v) = g(self, "StrokeAlignment") {
            s.align = names::stroke_align_in(&v);
        }
        if let Some(v) = g(self, "EndCap") {
            s.cap = names::cap_in(&v);
        }
        if let Some(v) = g(self, "EndJoin") {
            s.join = names::join_in(&v);
        }
        if let Some(v) = g(self, "MiterLimit").and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite()) {
            s.miter_limit = v;
        }
        if let Some(v) = g(self, "LeftLineEnd") {
            s.start = names::arrow_in(&v);
        }
        if let Some(v) = g(self, "RightLineEnd") {
            s.end = names::arrow_in(&v);
        }
        if let Some(v) = g(self, "GapColor") {
            s.gap_swatch = self.swatch_ref(&v);
        }
        if let Some(t) = g(self, "GapTint").and_then(|v| v.parse().ok()) {
            s.gap_tint = tint(Some(t)).unwrap_or(1.0);
        }
        s.overprint = g(self, "OverprintStroke").as_deref() == Some("true");
        s.gap_overprint = g(self, "OverprintGap").as_deref() == Some("true");
        s
    }

    fn item(&mut self, e: &El, parent: Affine) -> Option<Item> {
        let tag = e.local();
        let shape = match tag {
            "Rectangle" | "TextFrame" => Shape::Rectangle,
            "Oval" => Shape::Oval,
            "Polygon" => Shape::Polygon,
            "GraphicLine" => Shape::GraphicLine,
            "Group" => Shape::Group,
            _ => return None,
        };
        let id = ItemId(self.alloc());
        if let Some(s) = e.get("Self") {
            self.item_ids.insert(s.to_string(), id);
        }
        let layer = e.get("ItemLayer").and_then(|l| self.layer_ids.get(l)).copied().unwrap_or_else(|| self.layers[0].id);
        let path = e.prop_el("PathGeometry").map(path_of).unwrap_or_default();
        let mut it = Item::new(id, layer, shape, path);
        it.xf = parent * parse_affine(e.get("ItemTransform"));
        it.name = e.get("Name").filter(|n| *n != "$ID/").unwrap_or("").to_string();
        it.locked = e.get("Locked") == Some("true");
        it.hidden = e.get("Visible") == Some("false");
        it.overrides = e.get("OverriddenPageItem").and_then(|s| self.item_ids.get(s)).copied().filter(|i| self.is_parent_item(*i));
        it.nonprinting = e.get("Nonprinting") == Some("true");
        it.object_style = e
            .get("AppliedObjectStyle")
            .map(|r| {
                self.object_names
                    .get(r)
                    .cloned()
                    .unwrap_or_else(|| names::style_name_in(OBJECT_BUILTINS, &unescape_id(r.trim_start_matches("ObjectStyle/"))))
            })
            .unwrap_or_else(|| designcraft_doc::NO_OBJECT_STYLE.into());
        // Fill and stroke.
        if tag != "Group" {
            if let Some(f) = self.attr_or_style(e, "FillColor") {
                it.fill.swatch = self.swatch_ref(&f);
            }
            if let Some(t) = self.attr_or_style(e, "FillTint").and_then(|v| v.parse().ok()) {
                it.fill.tint = tint(Some(t)).unwrap_or(1.0);
            }
            it.fill.overprint = self.attr_or_style(e, "OverprintFill").as_deref() == Some("true");
            if let Some(a) = e.num("GradientFillAngle")
                && a != 0.0
            {
                it.fill.gradient_angle = Some(a);
            }
            let start: Option<Vec<f64>> = e.prop("GradientFillStart").map(|s| nums(&s));
            if let (Some([x, y]), Some(len)) = (start.as_deref().and_then(|v| <[f64; 2]>::try_from(v).ok()), e.num("GradientFillLength"))
                && len > 0.0
            {
                let a = e.num("GradientFillAngle").unwrap_or(0.0).to_radians();
                it.fill.gradient_vector = Some([x, y, x + len * a.cos(), y - len * a.sin()]);
            }
            it.stroke = self.stroke_from(e, Some(e));
            if it.stroke.weight <= 0.0 {
                it.stroke.swatch = swatch::NONE.into();
                it.stroke.weight = 1.0;
            }
            // A gradient on the stroke: its angle, or the Gradient Swatch tool's vector.
            if let Some(a) = e.num("GradientStrokeAngle")
                && a != 0.0
            {
                it.stroke.gradient_angle = Some(a);
            }
            let start: Option<Vec<f64>> = e.prop("GradientStrokeStart").map(|s| nums(&s));
            if let (Some([x, y]), Some(len)) = (start.as_deref().and_then(|v| <[f64; 2]>::try_from(v).ok()), e.num("GradientStrokeLength"))
                && len > 0.0
            {
                let a = e.num("GradientStrokeAngle").unwrap_or(0.0).to_radians();
                it.stroke.gradient_vector = Some([x, y, x + len * a.cos(), y - len * a.sin()]);
            }
        }
        // Corners: each corner's shape and radius come from the item, then its object style chain;
        // at each level a per-corner attribute wins over the legacy all-corners one.
        if tag != "Group" {
            let levels: Vec<&El> = std::iter::once(e).chain(self.object_style_chain(e)).collect();
            let corner =
                |n: &str, what: &str| levels.iter().find_map(|s| s.get(&format!("{n}Corner{what}")).or_else(|| s.get(&format!("Corner{what}"))));
            let mut corners = CornerOptions::default();
            for (i, n) in crate::export::corner_names(&it.path).iter().enumerate() {
                let shape = corner(n, "Option").map(names::corner_in).unwrap_or_default();
                let size = corner(n, "Radius").and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite()).unwrap_or(INDESIGN_CORNER_SIZE);
                corners.corners[i] = Corner { shape, size };
            }
            if !corners.is_none() {
                it.corners = corners;
            }
        }
        // Transparency.
        if let Some(t) = self.with_style(e, "TransparencySetting") {
            if let Some(bs) = t.find("BlendingSetting") {
                if let Some(o) = bs.num("Opacity") {
                    it.opacity = (o / 100.0).clamp(0.0, 1.0) as f32;
                }
                if let Some(m) = bs.get("BlendMode") {
                    it.blend = names::blend_in(m);
                }
            }
            self.effects_in(&t, &mut it.effects);
        }
        // Text wrap.
        if let Some(w) = self.with_style(e, "TextWrapPreference") {
            let mut tw = text_wrap(&w);
            if let Some(pg) = w.prop_el("PathGeometry") {
                match wrap_path_of(pg) {
                    Ok(p) => tw.path = Some(p),
                    Err(why) => log::warn!("IDML: custom text wrap boundary of {} dropped: {why}", e.get("Self").unwrap_or("an item")),
                }
            }
            it.wrap = tw;
        }
        // Type on a path.
        if tag != "TextFrame"
            && let Some(tp) = e.find("TextPath")
        {
            let story_self = tp.get("ParentStory").unwrap_or("n").to_string();
            if let Some(story) = self.story_ids.get(&story_self).copied() {
                let options = TextFrameOptions {
                    path: Some(designcraft_doc::PathType {
                        start: tp.num("StartBracket").unwrap_or(0.0).max(0.0),
                        // At (or past) the path's length it is the path's end: no end bracket.
                        end: tp
                            .num("EndBracket")
                            .filter(|e| e.is_finite() && *e >= 0.0)
                            .filter(|e| *e < designcraft_geom::warp::PathWarp::new(&it.path.to_bezpath(), false).length() - 0.01),
                        flip: tp.get("FlipPathEffect") == Some("Flipped"),
                        align: names::path_align_in(tp.get("TextAlignment").unwrap_or("")),
                    }),
                    ..Default::default()
                };
                it.content = Content::Text(TextFrame { story, options });
                self.ctx.threads.push((id, story_self, "n".into(), "n".into()));
            }
        }
        // Content.
        match tag {
            "TextFrame" => {
                let story_self = e.get("ParentStory").unwrap_or("n").to_string();
                let story = match self.story_ids.get(&story_self) {
                    Some(s) => *s,
                    None => {
                        let sid = StoryId(self.alloc());
                        self.stories.insert(sid, Story::new(sid));
                        self.story_ids.insert(format!("#orphan{}", sid.0), sid);
                        sid
                    }
                };
                // Options the frame doesn't set come from its object style chain.
                let prefs: Vec<&El> = std::iter::once(e).chain(self.object_style_chain(e)).filter_map(|s| s.find("TextFramePreference")).collect();
                let mut options = text_frame_options(&prefs);
                // `prefs` borrows the object styles, so the rule colour's swatch is resolved after it.
                if let Some(r) = Prefs(&prefs).prop("ColumnRuleStrokeColor") {
                    options.column_rule_color = self.swatch_ref(&r);
                }
                if let Some(g) = self.with_style(e, "BaselineFrameGridOption")
                    && g.get("UseCustomBaselineFrameGrid") == Some("true")
                {
                    options.baseline_grid = Some(designcraft_doc::FrameBaselineGrid {
                        start: g.num("StartingOffsetForBaselineFrameGrid").unwrap_or(0.0),
                        increment: g.num("BaselineFrameGridIncrement").unwrap_or(12.0),
                        relative_to: g.get("BaselineFrameGridRelativeOption").map(frame_grid_relative).unwrap_or_default(),
                    });
                }
                it.content = Content::Text(TextFrame { story, options });
                self.ctx.threads.push((
                    id,
                    story_self,
                    e.get("PreviousTextFrame").unwrap_or("n").to_string(),
                    e.get("NextTextFrame").unwrap_or("n").to_string(),
                ));
            }
            "Group" => {
                let mut kids = Vec::new();
                for c in e.elements() {
                    if let Some(k) = self.item(c, Affine::IDENTITY) {
                        kids.push(Arc::new(k));
                    }
                }
                it.content = Content::Group { items: kids };
                it.fill = Fill::none();
                it.stroke = Stroke::none();
            }
            _ => {
                if let Some(g) = e.elements().find(|c| matches!(c.local(), "Image" | "PDF" | "EPS" | "ImportedPage" | "WMF" | "PICT" | "SVG")) {
                    it.content = self.graphic(g);
                    let fill = self.image_fill(g);
                    if let Content::Graphic(gr) = &mut it.content {
                        gr.fill = fill;
                    }
                    if let (Some(ff), Content::Graphic(gr)) = (self.with_style(e, "FrameFittingOption"), &mut it.content) {
                        gr.crop = [ff.num("TopCrop"), ff.num("LeftCrop"), ff.num("BottomCrop"), ff.num("RightCrop")].map(|v| v.unwrap_or(0.0));
                        gr.fit_align = ff.get("FittingAlignment").map_or(4, names::anchor_in);
                        if ff.get("AutoFit") == Some("true") {
                            gr.auto_fit = ff.get("FittingOnEmptyFrame").map_or(designcraft_doc::Fitting::FillProportionally, names::fitting_in);
                            if gr.auto_fit == designcraft_doc::Fitting::None {
                                gr.auto_fit = designcraft_doc::Fitting::FillProportionally;
                            }
                        }
                    }
                } else {
                    // Page items pasted into this frame (Paste Into).
                    let kids: Vec<Arc<Item>> = e
                        .elements()
                        .filter(|c| matches!(c.local(), "Rectangle" | "Oval" | "Polygon" | "GraphicLine" | "Group" | "TextFrame"))
                        .filter_map(|c| self.item(c, Affine::IDENTITY))
                        .map(Arc::new)
                        .collect();
                    if !kids.is_empty() {
                        it.content = Content::Group { items: kids };
                    }
                }
            }
        }
        Some(it)
    }

    /// The swatch applied to a placed image (`FillColor`, `FillTint`): it colours a greyscale or
    /// bitmap image. Black at 100% is how InDesign writes an image nobody coloured: no swatch.
    fn image_fill(&mut self, g: &El) -> Fill {
        let Some(r) = g.get("FillColor") else { return Fill::none() };
        let swatch = self.swatch_ref(r);
        let tint = tint(g.num("FillTint")).unwrap_or(1.0);
        if swatch == swatch::BLACK && tint >= 1.0 {
            return Fill::none();
        }
        Fill { swatch, tint, ..Fill::none() }
    }

    fn graphic(&mut self, g: &El) -> Content {
        let gxf = parse_affine(g.get("ItemTransform"));
        let (l, t, r, b) = match g.prop_el("GraphicBounds") {
            Some(gb) => {
                (gb.num("Left").unwrap_or(0.0), gb.num("Top").unwrap_or(0.0), gb.num("Right").unwrap_or(0.0), gb.num("Bottom").unwrap_or(0.0))
            }
            None => (0.0, 0.0, 0.0, 0.0),
        };
        let size = ((r - l).abs(), (b - t).abs());
        let wrap =
            if g.get("AppliedObjectStyle").is_some() { self.with_style(g, "TextWrapPreference") } else { g.find("TextWrapPreference").cloned() };
        let wrap = wrap.map(|w| text_wrap(&w)).unwrap_or_default();
        let clip = clipping_path(g, Affine::translate((-l, -t)));
        let link = g.find("Link");
        let uri = link.and_then(|k| k.get("LinkResourceURI")).map(uri_to_path);
        let graphic = |asset, size| {
            Content::Graphic(designcraft_doc::Graphic {
                asset,
                size,
                xf: gxf * Affine::translate((l, t)),
                clip: clip.clone(),
                auto_fit: Default::default(),
                fit_align: 4,
                crop: [0.0; 4],
                // A wrap set on the placed graphic (Direct Selection) is stored on the graphic element,
                // over the graphic's own object style when it names one.
                wrap,
                fill: Default::default(),
            })
        };
        let mut data = g.prop_el("Contents").map(|c| base64_decode(&c.text_content())).unwrap_or_default();
        let page = pdf_page_in(g);
        // Where the box sits on the page needs the PDF parsed: the engine finds it after import.
        let pdf_crop = pdf_attribute(g).and_then(|a| a.get("PDFCrop")).map_or(designcraft_doc::PdfCrop::Crop, names::pdf_crop_in);
        let mut read_from = None;
        if data.is_empty()
            && let Some(p) = &uri
        {
            // Every placement of a linked file with one page and crop shares one asset; read the
            // file once.
            let key = (p.clone(), page, pdf_crop);
            if let Some(id) = self.linked_assets.get(&key) {
                let size = self.assets.get(id).and_then(|a| svg_viewbox_size(&a.data)).unwrap_or(size);
                return graphic(*id, size);
            }
            if let Some(d) = (self.read_link)(p) {
                data = d;
                read_from = Some(key);
            }
        }
        // A linked file that can't be read shows the document's preview of it, if it has one. The
        // preview only stands in for the file: format and pixel size still describe the link.
        let preview = data.is_empty().then(|| g.get("Self").and_then(|s| self.previews.get(s))).flatten();
        // InDesign draws a viewBox-only SVG at one unit to the point: its GraphicBounds are the
        // art's bounds, which can overhang the viewBox, so they must not size the viewBox.
        let size = svg_viewbox_size(&data).unwrap_or(size);
        let (mime, px) = if preview.is_some() { (None, None) } else { sniff_image(&data) };
        let mime = mime.map(str::to_string).unwrap_or_else(|| match link.and_then(|k| k.get("LinkResourceFormat")).unwrap_or("") {
            f if f.contains("EPS") || g.local() == "EPS" => "application/postscript".into(),
            f if f.contains("JPEG") => "image/jpeg".into(),
            f if f.contains("TIFF") => "image/tiff".into(),
            f if f.contains("PDF") || g.local() == "PDF" => "application/pdf".into(),
            f if f.contains("GIF") => "image/gif".into(),
            f if f.contains("PCX") => "image/pcx".into(),
            f if f.contains("WMF") || g.local() == "WMF" => "image/x-wmf".into(),
            _ => "image/png".into(),
        });
        let pixels = px.or_else(|| {
            let ppi = nums(g.get("ActualPpi").unwrap_or(""));
            (ppi.len() == 2 && size.0 > 0.0).then(|| ((size.0 * ppi[0] / 72.0).round() as u32, (size.1 * ppi[1] / 72.0).round() as u32))
        });
        let name =
            uri.as_deref().and_then(|p| p.rsplit(['/', '\\']).next()).filter(|n| !n.is_empty()).map(str::to_string).unwrap_or_else(|| "image".into());
        let fresh = AssetId(self.alloc());
        let link_path = uri.filter(|p| p.contains('/') || p.contains('\\'));
        let id = designcraft_doc::insert_asset(
            &mut self.assets,
            Asset {
                page,
                id: fresh,
                name,
                mime,
                link: link_path,
                data: preview.cloned().unwrap_or_else(|| Arc::new(data)),
                pixels,
                pdf_crop,
                ..Default::default()
            },
        );
        if let Some(key) = read_from {
            self.linked_assets.insert(key, id);
        }
        graphic(id, size)
    }

    fn link_threads(&mut self) {
        let threads = std::mem::take(&mut self.ctx.threads);
        let by_self: HashMap<String, ItemId> = self.item_ids.clone();
        let mut per_story: BTreeMap<StoryId, Vec<usize>> = BTreeMap::new();
        for (i, (id, story, _, _)) in threads.iter().enumerate() {
            let sid = self.story_ids.get(story).copied().or_else(|| self.frame_story(*id));
            if let Some(sid) = sid {
                per_story.entry(sid).or_default().push(i);
            }
        }
        for (sid, idx) in per_story {
            let members: HashSet<ItemId> = idx.iter().map(|i| threads[*i].0).collect();
            let next_of: HashMap<ItemId, ItemId> =
                idx.iter().filter_map(|i| by_self.get(&threads[*i].3).filter(|n| members.contains(n)).map(|n| (threads[*i].0, *n))).collect();
            let has_prev: HashSet<ItemId> = next_of.values().copied().collect();
            let mut order = Vec::new();
            let mut seen = HashSet::new();
            for i in &idx {
                let head = threads[*i].0;
                if has_prev.contains(&head) || seen.contains(&head) {
                    continue;
                }
                let mut cur = Some(head);
                while let Some(c) = cur {
                    if !seen.insert(c) {
                        break;
                    }
                    order.push(c);
                    cur = next_of.get(&c).copied();
                }
            }
            for i in &idx {
                let f = threads[*i].0;
                if seen.insert(f) {
                    order.push(f);
                }
            }
            if let Some(s) = self.stories.get_mut(&sid) {
                s.frames = order;
            }
        }
    }

    fn frame_story(&self, id: ItemId) -> Option<StoryId> {
        fn find(items: &[Arc<Item>], id: ItemId) -> Option<StoryId> {
            for it in items {
                let mut r = None;
                it.walk(&mut |i| {
                    if i.id == id {
                        r = i.text_frame().map(|t| t.story);
                    }
                });
                if r.is_some() {
                    return r;
                }
            }
            None
        }
        self.spreads.iter().chain(self.parents.iter()).find_map(|s| find(&s.items, id))
    }

    /// A nested style list replaces the one a style or paragraph inherits, and with it the drop
    /// cap style (the list's leading "through 1 Dropcap" entry, read into the drop cap style). A
    /// list of its own without that entry clears an inherited drop cap style.
    fn clear_inherited_drop_cap_styles(&mut self) {
        let clears = |a: &designcraft_doc::ParaAttrs| a.drop_cap_style.is_none() && a.nested_styles.is_some();
        let cleared: Vec<String> = self
            .styles
            .paragraph
            .iter()
            .filter(|s| clears(&s.para) && inherits_drop_cap_style(&self.styles, s.based_on.as_deref()))
            .map(|s| s.name.clone())
            .collect();
        for name in cleared {
            if let Some(s) = self.styles.para_mut(&name) {
                s.para.drop_cap_style = Some(st::NO_CHAR_STYLE.into());
            }
        }
        for story in self.stories.values_mut() {
            clear_story_drop_cap_styles(&self.styles, story, 0);
        }
    }

    fn finish(mut self, root: &El) -> Result<Document> {
        let title = root.get("Name").map(|n| n.trim_end_matches(".indd").to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "Untitled".into());
        if self.spreads.is_empty() {
            return Err(IdmlError::Invalid("document has no spreads".into()));
        }
        self.settings.lists = std::mem::take(&mut self.lists);
        self.clear_inherited_drop_cap_styles();
        // Make sure built-in paragraph styles exist and are first.
        let mut d = Document {
            title,
            settings: self.settings.clone(),
            spreads: std::mem::take(&mut self.spreads).into_iter().map(Arc::new).collect(),
            parents: std::mem::take(&mut self.parents).into_iter().map(Arc::new).collect(),
            layers: std::mem::take(&mut self.layers),
            stories: std::mem::take(&mut self.stories).into_iter().map(|(k, v)| (k, Arc::new(v))).collect(),
            styles: Arc::new(std::mem::take(&mut self.styles)),
            swatches: std::mem::take(&mut self.swatches),
            color_groups: std::mem::take(&mut self.color_groups),
            conditions: std::mem::take(&mut self.conditions),
            inks: std::mem::take(&mut self.inks),
            stroke_styles: std::mem::take(&mut self.stroke_styles),
            articles: vec![],
            xml: Default::default(),
            endnote_options: Default::default(),
            endnote_story: None,
            sections: std::mem::take(&mut self.sections),
            assets: std::mem::take(&mut self.assets),
            hyperlinks: vec![],
            data_merge: Default::default(),
            bookmarks: vec![],
            user_words: vec![],
            hyphenation_exceptions: vec![],
            toc: self.toc.take(),
            text_variables: self.text_vars.take().unwrap_or_else(designcraft_doc::vars::defaults),
            footnote_options: std::mem::take(&mut self.footnote_options),
            xref_formats: std::mem::take(&mut self.xref_formats),
            index: None,
            created: self.created.unwrap_or_else(designcraft_doc::vars::now),
            modified: self.modified.unwrap_or(0),
            next_id: self.next_id,
            font_scope: 0,
        };
        if let [x, y] = root.get("ZeroPoint").map(nums).unwrap_or_default()[..]
            && let Some(zp) = d.clamp_zero_point([x, y])
        {
            d.settings.zero_point = zp;
        }
        Ok(d)
    }
}
