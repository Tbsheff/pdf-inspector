//! Hyperlink and AcroForm field extraction.

use crate::types::{FormControl, FormControlKind, FormControlSource, ItemType, TextItem};
use lopdf::{Document, Object, ObjectId};
use std::collections::{HashMap, HashSet};

use super::fonts::{resolve_array, resolve_dict};
use super::get_number;

/// Upper bound on the number of form-field nodes visited during a single
/// `extract_form_fields` pass. A crafted PDF can chain thousands of distinct
/// `/Kids` fields to blow the stack even without an outright reference cycle,
/// so we cap total traversal work in addition to detecting cycles.
const MAX_FORM_FIELD_NODES: usize = 100_000;

/// Upper bound on `/Kids` recursion depth. Real AcroForm hierarchies are only
/// a few levels deep (fields → child fields → widgets); a crafted PDF can chain
/// tens of thousands of distinct fields into a linear `/Kids` list that would
/// overflow the stack via depth-first recursion long before the node budget is
/// reached. This depth cap bounds the stack independently of total node count.
const MAX_FORM_FIELD_DEPTH: usize = 100;

/// Traversal budget for the AcroForm field walk. Bounds both the number of
/// distinct nodes visited *and* the total number of `/Fields`/`/Kids` entries
/// examined.
///
/// Counting `visited` alone is not enough: invalid entries (non-references) and
/// duplicate references never grow `visited`, so an oversized array full of them
/// would iterate to completion no matter how large. Charging every examined
/// entry against the same budget makes it a real cap on traversal work.
pub(crate) struct FieldWalkBudget {
    visited: HashSet<ObjectId>,
    examined: usize,
}

impl FieldWalkBudget {
    fn new() -> Self {
        Self {
            visited: HashSet::new(),
            examined: 0,
        }
    }

    /// True once the budget is spent; callers must stop iterating and recursing.
    fn exhausted(&self) -> bool {
        self.visited.len() >= MAX_FORM_FIELD_NODES || self.examined >= MAX_FORM_FIELD_NODES
    }
}

pub fn extract_page_links(doc: &Document, page_id: ObjectId, page_num: u32) -> Vec<TextItem> {
    let mut links = Vec::new();

    // Try to get the page dictionary
    if let Ok(page_dict) = doc.get_dictionary(page_id) {
        // Get Annots array
        let annots = if let Ok(annots_ref) = page_dict.get(b"Annots") {
            if let Ok(obj_ref) = annots_ref.as_reference() {
                doc.get_object(obj_ref)
                    .ok()
                    .and_then(|o| o.as_array().ok().cloned())
            } else {
                annots_ref.as_array().ok().cloned()
            }
        } else {
            None
        };

        if let Some(annots) = annots {
            for annot_ref in annots {
                // Get annotation dictionary
                let annot_dict = if let Ok(obj_ref) = annot_ref.as_reference() {
                    doc.get_dictionary(obj_ref).ok()
                } else {
                    annot_ref.as_dict().ok()
                };

                if let Some(annot_dict) = annot_dict {
                    // Check if this is a Link annotation
                    if let Ok(subtype) = annot_dict.get(b"Subtype") {
                        if let Ok(subtype_name) = subtype.as_name() {
                            if subtype_name != b"Link" {
                                continue;
                            }
                        }
                    }

                    // Get the Rect (position)
                    let rect = if let Ok(rect_obj) = annot_dict.get(b"Rect") {
                        if let Ok(rect_array) = rect_obj.as_array() {
                            if rect_array.len() >= 4 {
                                let x1 = get_number(&rect_array[0]).unwrap_or(0.0);
                                let y1 = get_number(&rect_array[1]).unwrap_or(0.0);
                                let x2 = get_number(&rect_array[2]).unwrap_or(0.0);
                                let y2 = get_number(&rect_array[3]).unwrap_or(0.0);
                                Some((x1, y1, x2 - x1, y2 - y1))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // Get the action (A dictionary) or Dest
                    let uri = extract_link_uri(doc, annot_dict);

                    if let (Some((x, y, width, height)), Some(url)) = (rect, uri) {
                        links.push(TextItem {
                            text: url.clone(),
                            x,
                            y,
                            width,
                            height,
                            font: String::new(),
                            font_tag: String::new(),
                            font_size: 0.0,
                            page: page_num,
                            is_bold: false,
                            is_italic: false,
                            is_underline: false,
                            is_strikeout: false,
                            item_type: ItemType::Link(url),
                            mcid: None,
                        });
                    }
                }
            }
        }
    }

    links
}

/// Extract URI from a link annotation
pub(crate) fn extract_link_uri(doc: &Document, annot_dict: &lopdf::Dictionary) -> Option<String> {
    // Try to get the A (Action) dictionary
    if let Ok(action_ref) = annot_dict.get(b"A") {
        let action_dict = if let Ok(obj_ref) = action_ref.as_reference() {
            doc.get_dictionary(obj_ref).ok()
        } else {
            action_ref.as_dict().ok()
        };

        if let Some(action_dict) = action_dict {
            // Check for URI action
            if let Ok(uri_obj) = action_dict.get(b"URI") {
                if let Ok(uri_str) = uri_obj.as_str() {
                    return Some(String::from_utf8_lossy(uri_str).to_string());
                }
            }
        }
    }

    // Try Dest (named destination) - less common for external links
    // We'll skip this for now as it requires looking up named destinations

    None
}

pub(crate) struct FormExtraction {
    pub(crate) items: Vec<TextItem>,
    pub(crate) controls: Vec<FormControl>,
}

impl FormExtraction {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            controls: Vec::new(),
        }
    }
}

const FF_RADIO: i64 = 1 << 15;
const FF_PUSHBUTTON: i64 = 1 << 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonKind {
    Checkbox,
    Radio,
    PushButton,
}

fn button_kind(field_flags: i64) -> ButtonKind {
    if field_flags & FF_PUSHBUTTON != 0 {
        ButtonKind::PushButton
    } else if field_flags & FF_RADIO != 0 {
        ButtonKind::Radio
    } else {
        ButtonKind::Checkbox
    }
}

fn name_to_string(name: &[u8]) -> String {
    String::from_utf8_lossy(name).to_string()
}

fn appearance_state(widget: &lopdf::Dictionary) -> Option<String> {
    widget
        .get(b"AS")
        .ok()
        .and_then(|o| o.as_name().ok())
        .map(name_to_string)
}

fn widget_export_value(doc: &Document, widget: &lopdf::Dictionary) -> Option<String> {
    let appearances = widget.get(b"AP").ok().and_then(|o| resolve_dict(doc, o))?;
    let normal = appearances
        .get(b"N")
        .ok()
        .and_then(|o| resolve_dict(doc, o))?;
    normal
        .iter()
        .map(|(state, _)| name_to_string(state))
        .find(|state| state != OFF_STATE)
}

fn button_state_name(value: Option<&Object>) -> Option<String> {
    value.and_then(|v| v.as_name().ok()).map(name_to_string)
}

const OFF_STATE: &str = "Off";

fn appearance_state_is_on(widget: &lopdf::Dictionary) -> bool {
    appearance_state(widget).is_some_and(|state| state != OFF_STATE)
}

fn group_appearance_is_authoritative(doc: &Document, kids: &[Object]) -> bool {
    kids.iter()
        .filter_map(|kid| kid.as_reference().ok())
        .filter_map(|id| doc.get_dictionary(id).ok())
        .any(appearance_state_is_on)
}

fn widget_is_checked(
    appearance_is_authoritative: bool,
    own_appearance_state: Option<&str>,
    own_export_value: Option<&str>,
    inherited_selected_state: Option<&str>,
) -> bool {
    let appearance_on = own_appearance_state.is_some_and(|state| state != OFF_STATE);
    if appearance_is_authoritative {
        return appearance_on;
    }
    match (own_export_value, inherited_selected_state) {
        (Some(option), Some(selected)) => option == selected,
        (_, Some(selected)) => selected != OFF_STATE,
        (_, None) => appearance_on,
    }
}

pub(crate) fn extract_form_fields(
    doc: &Document,
    page_map: &HashMap<ObjectId, u32>,
) -> FormExtraction {
    let mut items = FormExtraction::new();

    // Navigate: trailer -> /Root -> /AcroForm -> /Fields
    let root = match doc.trailer.get(b"Root") {
        Ok(root_ref) => match root_ref.as_reference() {
            Ok(r) => match doc.get_dictionary(r) {
                Ok(d) => d,
                Err(_) => return items,
            },
            Err(_) => return items,
        },
        Err(_) => return items,
    };

    let acroform = match root.get(b"AcroForm") {
        Ok(obj) => match resolve_dict(doc, obj) {
            Some(d) => d,
            None => return items,
        },
        Err(_) => return items,
    };

    // Borrow the array rather than cloning it: a crafted `/Fields` can be huge,
    // and cloning would pay an O(n) allocation/copy before the budget check
    // below can stop the work.
    let fields = match acroform.get(b"Fields") {
        Ok(obj) => match resolve_array(doc, obj) {
            Some(arr) => arr,
            None => return items,
        },
        Err(_) => return items,
    };
    if fields.is_empty() {
        return items;
    }
    let annotation_pages = annotation_page_map(doc, page_map);

    // Bound the walk so a crafted PDF cannot send us into unbounded recursion
    // via a `/Kids` cycle, a deep chain, or an oversized array of invalid or
    // duplicate entries.
    let mut budget = FieldWalkBudget::new();

    for field_obj in fields {
        // Stop once the budget is spent so a `/Fields` array wider than the
        // budget can't burn CPU iterating entries whose walk would no-op. Charge
        // every entry (including invalid ones) against the budget.
        if budget.exhausted() {
            break;
        }
        budget.examined += 1;
        if let Ok(field_ref) = field_obj.as_reference() {
            walk_form_fields(
                doc,
                field_ref,
                InheritedField::default(),
                "",
                page_map,
                &annotation_pages,
                &mut items,
                &mut budget,
                0,
            );
        }
    }

    items
}

/// Map widget annotation objects back to the page whose `/Annots` array owns
/// them. Some valid widgets omit `/P`, so the page tree is the only reliable
/// ownership signal available for page-filtered extraction.
fn annotation_page_map(
    doc: &Document,
    page_map: &HashMap<ObjectId, u32>,
) -> HashMap<ObjectId, u32> {
    let mut annotation_pages = HashMap::new();
    for (&page_id, &page_num) in page_map {
        let Some(annotations) = doc
            .get_dictionary(page_id)
            .ok()
            .and_then(|page| page.get(b"Annots").ok())
            .and_then(|annotations| resolve_array(doc, annotations))
        else {
            continue;
        };
        for annotation in annotations {
            if let Ok(annotation_id) = annotation.as_reference() {
                annotation_pages.insert(annotation_id, page_num);
            }
        }
    }
    annotation_pages
}

#[derive(Clone, Copy, Default)]
struct InheritedField<'a> {
    field_type: Option<&'a [u8]>,
    field_flags: i64,
    value: Option<&'a Object>,
    tooltip: Option<&'a Object>,
    appearance_is_authoritative: bool,
}

/// Recursively walk the form field tree, extracting leaf field values.
#[allow(clippy::too_many_arguments)]
fn walk_form_fields(
    doc: &Document,
    field_id: ObjectId,
    parent: InheritedField<'_>,
    parent_name: &str,
    page_map: &HashMap<ObjectId, u32>,
    annotation_pages: &HashMap<ObjectId, u32>,
    items: &mut FormExtraction,
    budget: &mut FieldWalkBudget,
    depth: usize,
) {
    // Guard against `/Kids` cycles and pathologically large field trees.
    // Exceeding the depth cap means the chain is too deep to be a legitimate
    // form (and would overflow the stack); an exhausted budget means the tree is
    // too large. Both checks run *before* inserting so the visited set can never
    // grow past the budget.
    if depth > MAX_FORM_FIELD_DEPTH || budget.exhausted() {
        return;
    }
    // Revisiting an object ID means we hit a `/Kids` cycle.
    if !budget.visited.insert(field_id) {
        return;
    }

    let field_dict = match doc.get_dictionary(field_id) {
        Ok(d) => d,
        Err(_) => return,
    };

    // Build fully qualified field name
    let local_name = field_dict
        .get(b"T")
        .ok()
        .and_then(|o| o.as_str().ok())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .unwrap_or_default();

    let full_name = if parent_name.is_empty() {
        local_name.clone()
    } else if local_name.is_empty() {
        parent_name.to_string()
    } else {
        format!("{}.{}", parent_name, local_name)
    };

    // Determine field type (may be inherited from parent)
    let inherited = InheritedField {
        field_type: field_dict
            .get(b"FT")
            .ok()
            .and_then(|o| o.as_name().ok())
            .or(parent.field_type),
        field_flags: field_dict
            .get(b"Ff")
            .ok()
            .and_then(|o| o.as_i64().ok())
            .unwrap_or(parent.field_flags),
        value: field_dict.get(b"V").ok().or(parent.value),
        tooltip: field_dict.get(b"TU").ok().or(parent.tooltip),
        appearance_is_authoritative: parent.appearance_is_authoritative
            || appearance_state_is_on(field_dict),
    };
    let mut inherited = inherited;
    let ft = inherited.field_type;

    // Check for /Kids — if present, recurse into children
    if let Ok(kids_obj) = field_dict.get(b"Kids") {
        // Iterate the borrowed array directly — cloning a crafted, oversized
        // `/Kids` would allocate and copy every entry before the budget check
        // below could stop the work.
        if let Some(kids) = resolve_array(doc, kids_obj) {
            inherited.appearance_is_authoritative |= group_appearance_is_authoritative(doc, kids);
            for kid in kids {
                // Stop once the budget is spent so a `/Kids` array wider than the
                // budget can't burn CPU iterating entries whose walk would no-op.
                // Charge every entry (including invalid/duplicate ones) against
                // the budget so this is a true traversal-work cap.
                if budget.exhausted() {
                    break;
                }
                budget.examined += 1;
                if let Ok(kid_ref) = kid.as_reference() {
                    walk_form_fields(
                        doc,
                        kid_ref,
                        inherited,
                        &full_name,
                        page_map,
                        annotation_pages,
                        items,
                        budget,
                        depth + 1,
                    );
                }
            }
            return;
        }
    }

    // Leaf field — extract value
    let ft = match ft {
        Some(ft) => ft,
        None => return,
    };

    // Skip signature fields
    if ft == b"Sig" {
        return;
    }

    let (x, y, width, height) = match field_dict.get(b"Rect") {
        Ok(rect_obj) => match rect_obj.as_array() {
            Ok(rect_array) if rect_array.len() >= 4 => {
                let x1 = get_number(&rect_array[0]).unwrap_or(0.0);
                let y1 = get_number(&rect_array[1]).unwrap_or(0.0);
                let x2 = get_number(&rect_array[2]).unwrap_or(0.0);
                let y2 = get_number(&rect_array[3]).unwrap_or(0.0);
                (x1.min(x2), y1.min(y2), (x2 - x1).abs(), (y2 - y1).abs())
            }
            _ => (0.0, 0.0, 0.0, 0.0),
        },
        Err(_) => (0.0, 0.0, 0.0, 0.0),
    };

    let page_num = field_dict
        .get(b"P")
        .ok()
        .and_then(|o| o.as_reference().ok())
        .and_then(|p| page_map.get(&p).copied())
        .or_else(|| annotation_pages.get(&field_id).copied())
        .unwrap_or(1);

    if ft == b"Btn" {
        let kind = match button_kind(inherited.field_flags) {
            ButtonKind::PushButton => return,
            ButtonKind::Radio => FormControlKind::Radio,
            ButtonKind::Checkbox => FormControlKind::Checkbox,
        };
        let export_value = widget_export_value(doc, field_dict);
        let checked = widget_is_checked(
            inherited.appearance_is_authoritative,
            appearance_state(field_dict).as_deref(),
            export_value.as_deref(),
            button_state_name(inherited.value).as_deref(),
        );

        items.controls.push(FormControl {
            name: full_name,
            kind,
            export_value,
            checked,
            label: None,
            tooltip: inherited
                .tooltip
                .and_then(|o| o.as_str().ok())
                .map(|s| String::from_utf8_lossy(s).to_string())
                .filter(|s| !s.is_empty()),
            source: FormControlSource::AcroForm,
            page: page_num,
            x,
            y,
            width,
            height,
        });
        return;
    }

    // Get field value
    let value = match inherited.value {
        Some(v) => v,
        None => return,
    };

    let value_str = match ft {
        b"Tx" | b"Ch" => {
            // Text or Choice field — value is a string or array of strings
            match value {
                Object::String(s, _) => {
                    let s = String::from_utf8_lossy(s).to_string();
                    if s.is_empty() {
                        return;
                    }
                    s
                }
                Object::Array(arr) => {
                    let parts: Vec<String> = arr
                        .iter()
                        .filter_map(|o| {
                            if let Object::String(s, _) = o {
                                Some(String::from_utf8_lossy(s).to_string())
                            } else {
                                None
                            }
                        })
                        .collect();
                    if parts.is_empty() {
                        return;
                    }
                    parts.join(", ")
                }
                _ => return,
            }
        }
        b"Btn" => {
            // Checkbox/radio — value is a name
            match value.as_name() {
                Ok(name) if name == b"Off" => return,
                Ok(name) => {
                    let name_str = String::from_utf8_lossy(name).to_string();
                    if name_str == "Yes" || name_str == "1" {
                        "Yes".to_string()
                    } else {
                        name_str
                    }
                }
                Err(_) => return,
            }
        }
        _ => return,
    };

    let text = if full_name.is_empty() {
        value_str
    } else {
        format!("{}: {}", full_name, value_str)
    };

    items.items.push(TextItem {
        text,
        x,
        y,
        width,
        height,
        font: String::new(),
        font_tag: String::new(),
        font_size: 0.0,
        page: page_num,
        is_bold: false,
        is_italic: false,
        is_underline: false,
        is_strikeout: false,
        item_type: ItemType::FormField,
        mcid: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Object};

    #[test]
    fn widget_without_page_reference_uses_owning_page_annotation() {
        let mut doc = Document::new();
        let widget_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Tx",
            "T" => Object::string_literal("customer"),
            "V" => Object::string_literal("Alice"),
            "Rect" => vec![10.into(), 20.into(), 110.into(), 40.into()],
        });
        let page_one_id = doc.add_object(dictionary! {
            "Type" => "Page",
        });
        let page_two_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Annots" => vec![Object::Reference(widget_id)],
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(widget_id)],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let page_map = HashMap::from([(page_one_id, 1), (page_two_id, 2)]);
        let items = extract_form_fields(&doc, &page_map);

        assert_eq!(items.items.len(), 1);
        assert_eq!(items.items[0].page, 2);
        assert_eq!(items.items[0].text, "customer: Alice");
    }

    #[test]
    fn kids_self_cycle_does_not_overflow_stack() {
        // A crafted AcroForm field that lists itself in `/Kids` must not send
        // the traversal into unbounded recursion.
        let mut doc = Document::new();
        let field_id = doc.new_object_id();
        doc.set_object(
            field_id,
            dictionary! {
                "FT" => "Tx",
                "T" => Object::string_literal("loop"),
                "Kids" => vec![Object::Reference(field_id)],
            },
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(field_id)],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let page_map = HashMap::new();
        // Completes (rather than overflowing the stack) and yields no items.
        let items = extract_form_fields(&doc, &page_map);
        assert!(items.items.is_empty());
    }

    #[test]
    fn kids_mutual_cycle_terminates() {
        // Two fields that reference each other via `/Kids` form a cycle that
        // must also terminate.
        let mut doc = Document::new();
        let field_a = doc.new_object_id();
        let field_b = doc.new_object_id();
        doc.set_object(
            field_a,
            dictionary! {
                "T" => Object::string_literal("a"),
                "Kids" => vec![Object::Reference(field_b)],
            },
        );
        doc.set_object(
            field_b,
            dictionary! {
                "T" => Object::string_literal("b"),
                "Kids" => vec![Object::Reference(field_a)],
            },
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(field_a)],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let page_map = HashMap::new();
        let items = extract_form_fields(&doc, &page_map);
        assert!(items.items.is_empty());
    }

    #[test]
    fn deep_acyclic_kids_chain_does_not_overflow_stack() {
        // A long chain of *distinct* fields (no cycle) must also terminate:
        // the visited set alone would still recurse to the chain length, so
        // the depth cap is what prevents a stack overflow here.
        let mut doc = Document::new();
        let n = MAX_FORM_FIELD_DEPTH * 500;
        let ids: Vec<ObjectId> = (0..=n).map(|_| doc.new_object_id()).collect();
        for i in 0..n {
            doc.set_object(
                ids[i],
                dictionary! {
                    "FT" => "Tx",
                    "Kids" => vec![Object::Reference(ids[i + 1])],
                },
            );
        }
        // Leaf carries a value; it sits far below the depth cap so it is never
        // reached, proving traversal stops early rather than crashing.
        doc.set_object(
            ids[n],
            dictionary! {
                "FT" => "Tx",
                "T" => Object::string_literal("leaf"),
                "V" => Object::string_literal("x"),
                "Rect" => vec![10.into(), 20.into(), 110.into(), 40.into()],
            },
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(ids[0])],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let page_map = HashMap::new();
        let items = extract_form_fields(&doc, &page_map);
        assert!(items.items.is_empty());
    }

    #[test]
    fn wide_tree_traversal_stops_at_node_budget() {
        // A single field with a `/Kids` array wider than the node budget must
        // stop traversal at the cap rather than growing `visited` (and the work)
        // without bound. Each processed leaf emits one item, so the item count
        // is bounded by the budget and reaches right up to it (a couple of
        // slots go to the root and the boundary node charged against the cap).
        let mut doc = Document::new();
        let fanout = MAX_FORM_FIELD_NODES + 50;
        let leaf_ids: Vec<ObjectId> = (0..fanout).map(|_| doc.new_object_id()).collect();
        for &leaf in &leaf_ids {
            doc.set_object(
                leaf,
                dictionary! {
                    "FT" => "Tx",
                    "V" => Object::string_literal("v"),
                    "Rect" => vec![10.into(), 20.into(), 110.into(), 40.into()],
                },
            );
        }
        let kids: Vec<Object> = leaf_ids.iter().map(|&id| Object::Reference(id)).collect();
        let root_id = doc.add_object(dictionary! {
            "T" => Object::string_literal("root"),
            "Kids" => kids,
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(root_id)],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let page_map = HashMap::new();
        let items = extract_form_fields(&doc, &page_map);
        // Extraction stops at the budget: bounded above by the cap, and it gets
        // right up to it (allowing a small delta for the root/boundary nodes
        // charged against the budget).
        assert!(items.items.len() <= MAX_FORM_FIELD_NODES);
        assert!(items.items.len() >= MAX_FORM_FIELD_NODES - 3);
    }

    #[test]
    fn wide_top_level_fields_stop_at_node_budget() {
        // A top-level `/Fields` array wider than the budget must also stop at
        // the cap: the item count is bounded by the budget and reaches right up
        // to it.
        let mut doc = Document::new();
        let fanout = MAX_FORM_FIELD_NODES + 50;
        let leaf_ids: Vec<ObjectId> = (0..fanout).map(|_| doc.new_object_id()).collect();
        for &leaf in &leaf_ids {
            doc.set_object(
                leaf,
                dictionary! {
                    "FT" => "Tx",
                    "V" => Object::string_literal("v"),
                    "Rect" => vec![10.into(), 20.into(), 110.into(), 40.into()],
                },
            );
        }
        let fields: Vec<Object> = leaf_ids.iter().map(|&id| Object::Reference(id)).collect();
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => fields,
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let page_map = HashMap::new();
        let items = extract_form_fields(&doc, &page_map);
        assert!(items.items.len() <= MAX_FORM_FIELD_NODES);
        assert!(items.items.len() >= MAX_FORM_FIELD_NODES - 3);
    }

    #[test]
    fn duplicate_and_invalid_kids_entries_stop_at_budget() {
        // Duplicate references and non-reference junk never grow `visited`, so
        // without charging examined entries against the budget an oversized
        // array of them would iterate to completion. The walk must still
        // terminate and extract the single real leaf exactly once.
        let mut doc = Document::new();
        let leaf_id = doc.new_object_id();
        doc.set_object(
            leaf_id,
            dictionary! {
                "FT" => "Tx",
                "V" => Object::string_literal("v"),
                "Rect" => vec![10.into(), 20.into(), 110.into(), 40.into()],
            },
        );
        // A `/Kids` array far wider than the budget: half duplicate references
        // to the same leaf, half invalid (null) entries.
        let mut kids: Vec<Object> = Vec::new();
        for i in 0..(MAX_FORM_FIELD_NODES * 2) {
            if i % 2 == 0 {
                kids.push(Object::Reference(leaf_id));
            } else {
                kids.push(Object::Null);
            }
        }
        let root_id = doc.add_object(dictionary! {
            "T" => Object::string_literal("root"),
            "Kids" => kids,
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(root_id)],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let page_map = HashMap::new();
        let items = extract_form_fields(&doc, &page_map);
        assert_eq!(items.items.len(), 1);
    }

    fn radio_widget(doc: &mut Document, option: &str, x: i64) -> ObjectId {
        doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "Rect" => vec![x.into(), 100.into(), (x + 12).into(), 112.into()],
            "AS" => Object::Name(b"Off".to_vec()),
            "AP" => dictionary! {
                "N" => dictionary! {
                    "Off" => Object::Null,
                    option => Object::Null,
                },
            },
        })
    }

    fn radio_group_document(selected: &str, options: &[&str]) -> Document {
        let mut doc = Document::new();
        let kids: Vec<Object> = options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                Object::Reference(radio_widget(&mut doc, option, 100 + index as i64 * 20))
            })
            .collect();
        let group_id = doc.add_object(dictionary! {
            "FT" => "Btn",
            "T" => Object::string_literal("M1860"),
            "TU" => Object::string_literal("Ambulation and locomotion"),
            "Ff" => Object::Integer(FF_RADIO),
            "V" => Object::Name(selected.as_bytes().to_vec()),
            "Kids" => kids,
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(group_id)],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc
    }

    #[test]
    fn radio_group_emits_one_control_per_widget_with_parent_value() {
        let doc = radio_group_document("3", &["0", "1", "2", "3", "4"]);
        let controls = extract_form_fields(&doc, &HashMap::new()).controls;

        assert_eq!(controls.len(), 5);
        assert!(controls
            .iter()
            .all(|c| c.kind == FormControlKind::Radio && c.name == "M1860"));
        assert_eq!(
            controls
                .iter()
                .map(|c| c.export_value.clone().unwrap())
                .collect::<Vec<_>>(),
            vec!["0", "1", "2", "3", "4"]
        );
        assert_eq!(
            controls.iter().map(|c| c.checked).collect::<Vec<_>>(),
            vec![false, false, false, true, false]
        );
        assert_eq!(
            controls[0].tooltip.as_deref(),
            Some("Ambulation and locomotion")
        );
    }

    #[test]
    fn widget_appearance_state_overrides_inherited_value() {
        let mut doc = radio_group_document("1", &["0", "1"]);
        let widget_id = doc
            .objects
            .iter()
            .find(|(_, object)| {
                object.as_dict().is_ok_and(|d| {
                    d.get(b"Subtype")
                        .and_then(|s| s.as_name())
                        .is_ok_and(|n| n == b"Widget")
                })
            })
            .map(|(id, _)| *id)
            .expect("widget");
        if let Ok(widget) = doc.get_object_mut(widget_id).and_then(|o| o.as_dict_mut()) {
            widget.set("AS", Object::Name(b"0".to_vec()));
        }

        let controls = extract_form_fields(&doc, &HashMap::new()).controls;
        let checked: Vec<&FormControl> = controls.iter().filter(|c| c.checked).collect();
        assert_eq!(checked.len(), 1);
        assert_eq!(checked[0].export_value.as_deref(), Some("0"));
    }

    #[test]
    fn unchecked_checkbox_is_still_reported() {
        let mut doc = Document::new();
        let field_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Btn",
            "T" => Object::string_literal("fall_risk"),
            "V" => Object::Name(b"Off".to_vec()),
            "AS" => Object::Name(b"Off".to_vec()),
            "Rect" => vec![10.into(), 20.into(), 22.into(), 32.into()],
            "AP" => dictionary! {
                "N" => dictionary! {
                    "Off" => Object::Null,
                    "Yes" => Object::Null,
                },
            },
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(field_id)],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let controls = extract_form_fields(&doc, &HashMap::new()).controls;
        assert_eq!(controls.len(), 1);
        assert_eq!(controls[0].kind, FormControlKind::Checkbox);
        assert_eq!(controls[0].export_value.as_deref(), Some("Yes"));
        assert!(!controls[0].checked);
    }

    #[test]
    fn pushbutton_is_not_a_form_control() {
        let mut doc = Document::new();
        let field_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Btn",
            "T" => Object::string_literal("submit"),
            "Ff" => Object::Integer(FF_PUSHBUTTON),
            "Rect" => vec![10.into(), 20.into(), 60.into(), 40.into()],
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(field_id)],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let extraction = extract_form_fields(&doc, &HashMap::new());
        assert!(extraction.controls.is_empty());
        assert!(extraction.items.is_empty());
    }

    #[test]
    fn export_value_can_be_a_custom_name() {
        let doc = radio_group_document("Independent", &["Independent", "NeedsHelp"]);
        let controls = extract_form_fields(&doc, &HashMap::new()).controls;
        assert_eq!(controls[0].export_value.as_deref(), Some("Independent"));
        assert!(controls[0].checked);
        assert!(!controls[1].checked);
    }
}
