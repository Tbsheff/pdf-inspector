use lopdf::content::Operation;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use std::collections::{HashMap, HashSet};

use crate::types::{FormControl, FormControlKind, FormControlSource, ItemType, TextItem};

use super::fonts::resolve_dict;
use super::links::extract_form_fields;
use super::{get_number, image_bbox_from_ctm, multiply_matrices};

const MIN_STAMP_POINTS: f32 = 4.0;
const MAX_STAMP_POINTS: f32 = 24.0;
const MIN_STAMP_ASPECT: f32 = 0.5;
const MAX_STAMP_ASPECT: f32 = 2.0;
const MAX_STAMP_PIXELS: i64 = 64;

const STRONG_INK_LUMA: u8 = 128;
const ANY_INK_LUMA: u8 = 220;
const CHECKED_INTERIOR_RATIO: f32 = 0.06;
const OUTLINE_PERIMETER_RATIO: f32 = 0.5;
const SOLID_BLOCK_RATIO: f32 = 0.9;
const INTERIOR_INSET_FRACTION: f32 = 0.25;

const MIN_STAMPS_PER_PAGE: usize = 3;

const MAX_XOBJECT_DEPTH: usize = 8;
const MAX_CONTENT_OPERATIONS: usize = 400_000;

const LABEL_MAX_DISTANCE: f32 = 150.0;
const LABEL_BAND_TOLERANCE: f32 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StampShape {
    Checked,
    Unchecked,
}

struct StampPlacement {
    shape: StampShape,
    page: u32,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

pub(crate) fn extract_form_controls(doc: &Document, items: &[TextItem]) -> Vec<FormControl> {
    let page_map: HashMap<ObjectId, u32> = doc
        .get_pages()
        .into_iter()
        .map(|(number, id)| (id, number))
        .collect();

    let mut controls = extract_form_fields(doc, &page_map).controls;

    let widget_pages: HashSet<u32> = controls.iter().map(|control| control.page).collect();
    controls.extend(
        stamp_controls(doc)
            .into_iter()
            .filter(|control| !widget_pages.contains(&control.page)),
    );

    attach_labels(&mut controls, items);
    controls.sort_by(|a, b| {
        a.page
            .cmp(&b.page)
            .then(b.y.total_cmp(&a.y))
            .then(a.x.total_cmp(&b.x))
    });
    controls
}

fn stamp_sized_image_ids(doc: &Document) -> HashSet<ObjectId> {
    doc.objects
        .iter()
        .filter_map(|(id, object)| Some((id, object.as_stream().ok()?)))
        .filter(|(_, stream)| {
            let is_image = stream
                .dict
                .get(b"Subtype")
                .ok()
                .and_then(|o| o.as_name().ok())
                .is_some_and(|name| name == b"Image");
            let side = |key: &[u8]| stream.dict.get(key).ok().and_then(|o| o.as_i64().ok());
            let fits = matches!(
                (side(b"Width"), side(b"Height")),
                (Some(w), Some(h)) if (2..=MAX_STAMP_PIXELS).contains(&w)
                    && (2..=MAX_STAMP_PIXELS).contains(&h)
            );
            is_image && fits
        })
        .map(|(id, _)| *id)
        .collect()
}

fn stamp_controls(doc: &Document) -> Vec<FormControl> {
    let candidates = stamp_sized_image_ids(doc);
    if candidates.is_empty() {
        return Vec::new();
    }

    let mut classified: HashMap<ObjectId, Option<StampShape>> = HashMap::new();
    let mut by_page: HashMap<u32, Vec<StampPlacement>> = HashMap::new();

    for (page_num, page_id) in doc.get_pages() {
        let Ok(content) = doc.get_and_decode_page_content(page_id) else {
            continue;
        };
        let xobjects = page_xobjects(doc, page_id);
        let mut budget = MAX_CONTENT_OPERATIONS;
        let mut placements = Vec::new();
        collect_stamp_placements(
            doc,
            &content.operations,
            &xobjects,
            &candidates,
            &[1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            page_num,
            &mut classified,
            &mut placements,
            &mut budget,
            0,
        );
        if placements.len() >= MIN_STAMPS_PER_PAGE {
            by_page.insert(page_num, placements);
        } else if !placements.is_empty() {
            log::debug!(
                "page {page_num}: dropped {} checkbox stamp(s), fewer than the {MIN_STAMPS_PER_PAGE} needed to treat them as a form",
                placements.len()
            );
        }
    }

    let mut controls = Vec::new();
    for placements in by_page.into_values() {
        for placement in placements {
            controls.push(FormControl {
                name: String::new(),
                kind: FormControlKind::Checkbox,
                export_value: None,
                checked: placement.shape == StampShape::Checked,
                label: None,
                tooltip: None,
                source: FormControlSource::StampImage,
                page: placement.page,
                x: placement.x,
                y: placement.y,
                width: placement.width,
                height: placement.height,
            });
        }
    }
    controls
}

fn page_xobjects(doc: &Document, page_id: ObjectId) -> HashMap<String, ObjectId> {
    let Ok((page_dict, inherited)) = doc.get_page_resources(page_id) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    if let Some(dict) = page_dict {
        collect_xobject_ids(doc, dict, &mut out);
    }
    for id in inherited {
        if let Ok(dict) = doc.get_dictionary(id) {
            collect_xobject_ids(doc, dict, &mut out);
        }
    }
    out
}

fn resource_xobjects(doc: &Document, resources: &Dictionary) -> HashMap<String, ObjectId> {
    let mut out = HashMap::new();
    collect_xobject_ids(doc, resources, &mut out);
    out
}

fn collect_xobject_ids(
    doc: &Document,
    resources: &Dictionary,
    out: &mut HashMap<String, ObjectId>,
) {
    let Some(xobjects) = resources
        .get(b"XObject")
        .ok()
        .and_then(|o| resolve_dict(doc, o))
    else {
        return;
    };
    for (name, value) in xobjects.iter() {
        if let Ok(id) = value.as_reference() {
            out.insert(String::from_utf8_lossy(name).to_string(), id);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_stamp_placements(
    doc: &Document,
    operations: &[Operation],
    xobjects: &HashMap<String, ObjectId>,
    candidates: &HashSet<ObjectId>,
    base_ctm: &[f32; 6],
    page_num: u32,
    classified: &mut HashMap<ObjectId, Option<StampShape>>,
    placements: &mut Vec<StampPlacement>,
    budget: &mut usize,
    depth: usize,
) {
    let mut ctm = *base_ctm;
    let mut stack: Vec<[f32; 6]> = Vec::new();

    for op in operations {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        match op.operator.as_str() {
            "q" => stack.push(ctm),
            "Q" => {
                if let Some(saved) = stack.pop() {
                    ctm = saved;
                }
            }
            "cm" if op.operands.len() >= 6 => {
                let mut m = [0.0f32; 6];
                for (i, operand) in op.operands.iter().take(6).enumerate() {
                    m[i] = get_number(operand).unwrap_or(0.0);
                }
                ctm = multiply_matrices(&m, &ctm);
            }
            "Do" => {
                let Some(name) = op.operands.first().and_then(|o| o.as_name().ok()) else {
                    continue;
                };
                let Some(&id) = xobjects.get(String::from_utf8_lossy(name).as_ref()) else {
                    continue;
                };
                let Ok(stream) = doc.get_object(id).and_then(|o| o.as_stream()) else {
                    continue;
                };
                match stream
                    .dict
                    .get(b"Subtype")
                    .ok()
                    .and_then(|o| o.as_name().ok())
                {
                    Some(b"Image") => {
                        let (x, y, width, height) = image_bbox_from_ctm(&ctm);
                        if !candidates.contains(&id) || !is_stamp_sized(width, height) {
                            continue;
                        }
                        let shape = *classified
                            .entry(id)
                            .or_insert_with(|| classify_stamp_image(doc, stream));
                        if let Some(shape) = shape {
                            placements.push(StampPlacement {
                                shape,
                                page: page_num,
                                x,
                                y,
                                width,
                                height,
                            });
                        }
                    }
                    Some(b"Form") if depth < MAX_XOBJECT_DEPTH => {
                        let Ok(content) = lopdf::content::Content::decode(
                            &stream
                                .decompressed_content()
                                .unwrap_or(stream.content.clone()),
                        ) else {
                            continue;
                        };
                        let nested_ctm = multiply_matrices(&form_matrix(stream), &ctm);
                        let nested_xobjects = stream
                            .dict
                            .get(b"Resources")
                            .ok()
                            .and_then(|o| resolve_dict(doc, o))
                            .map(|resources| resource_xobjects(doc, resources))
                            .unwrap_or_default();
                        collect_stamp_placements(
                            doc,
                            &content.operations,
                            &nested_xobjects,
                            candidates,
                            &nested_ctm,
                            page_num,
                            classified,
                            placements,
                            budget,
                            depth + 1,
                        );
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn form_matrix(stream: &Stream) -> [f32; 6] {
    let mut matrix = [1.0f32, 0.0, 0.0, 1.0, 0.0, 0.0];
    if let Some(values) = stream
        .dict
        .get(b"Matrix")
        .ok()
        .and_then(|o| o.as_array().ok())
    {
        if values.len() >= 6 {
            for (i, value) in values.iter().take(6).enumerate() {
                matrix[i] = get_number(value).unwrap_or(matrix[i]);
            }
        }
    }
    matrix
}

fn is_stamp_sized(width: f32, height: f32) -> bool {
    let in_range = |v: f32| (MIN_STAMP_POINTS..=MAX_STAMP_POINTS).contains(&v);
    if !in_range(width) || !in_range(height) {
        return false;
    }
    let aspect = width / height;
    (MIN_STAMP_ASPECT..=MAX_STAMP_ASPECT).contains(&aspect)
}

struct LumaGrid {
    width: i64,
    height: i64,
    samples: Vec<u8>,
}

impl LumaGrid {
    fn at(&self, row: i64, col: i64) -> u8 {
        self.samples[(row * self.width + col) as usize]
    }

    fn interior_strong_ink(&self) -> Option<f32> {
        let inset_x = ((self.width as f32 * INTERIOR_INSET_FRACTION).round() as i64).max(1);
        let inset_y = ((self.height as f32 * INTERIOR_INSET_FRACTION).round() as i64).max(1);
        if self.width - 2 * inset_x < 1 || self.height - 2 * inset_y < 1 {
            return None;
        }
        let mut total = 0usize;
        let mut inked = 0usize;
        for row in inset_y..self.height - inset_y {
            for col in inset_x..self.width - inset_x {
                total += 1;
                if self.at(row, col) < STRONG_INK_LUMA {
                    inked += 1;
                }
            }
        }
        Some(inked as f32 / total as f32)
    }

    fn ring_ink(&self, depth: i64, max_luma: u8) -> Option<f32> {
        if self.width - 2 * depth < 2 || self.height - 2 * depth < 2 {
            return None;
        }
        let mut total = 0usize;
        let mut inked = 0usize;
        for row in depth..self.height - depth {
            for col in depth..self.width - depth {
                let on_ring = row == depth
                    || row == self.height - depth - 1
                    || col == depth
                    || col == self.width - depth - 1;
                if !on_ring {
                    continue;
                }
                total += 1;
                if self.at(row, col) < max_luma {
                    inked += 1;
                }
            }
        }
        Some(inked as f32 / total as f32)
    }

    fn outline_ink(&self, max_luma: u8) -> f32 {
        (0..=1)
            .filter_map(|depth| self.ring_ink(depth, max_luma))
            .fold(0.0, f32::max)
    }
}

fn classify_stamp_image(doc: &Document, stream: &Stream) -> Option<StampShape> {
    classify_luma_grid(&decode_luma_grid(doc, stream)?)
}

fn classify_luma_grid(grid: &LumaGrid) -> Option<StampShape> {
    let interior_ink = grid.interior_strong_ink()?;
    let outline_ink = grid.outline_ink(ANY_INK_LUMA);
    let outline_solid = grid.outline_ink(STRONG_INK_LUMA);

    let is_solid_block = interior_ink > SOLID_BLOCK_RATIO && outline_solid > SOLID_BLOCK_RATIO;
    if is_solid_block {
        return None;
    }
    if interior_ink >= CHECKED_INTERIOR_RATIO {
        return Some(StampShape::Checked);
    }
    if outline_ink >= OUTLINE_PERIMETER_RATIO {
        return Some(StampShape::Unchecked);
    }
    None
}

fn decode_luma_grid(doc: &Document, stream: &Stream) -> Option<LumaGrid> {
    let width = stream.dict.get(b"Width").ok()?.as_i64().ok()?;
    let height = stream.dict.get(b"Height").ok()?.as_i64().ok()?;
    if width < 2 || height < 2 || width > MAX_STAMP_PIXELS || height > MAX_STAMP_PIXELS {
        return None;
    }

    let bits = stream
        .dict
        .get(b"BitsPerComponent")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(8);
    let is_mask = stream
        .dict
        .get(b"ImageMask")
        .ok()
        .and_then(|o| o.as_bool().ok())
        .unwrap_or(false);
    let components = if is_mask {
        1
    } else {
        colorspace_components(doc, stream)?
    };
    let data = stream.decompressed_content().ok()?;

    let inverted = stream
        .dict
        .get(b"Decode")
        .ok()
        .and_then(|o| o.as_array().ok())
        .and_then(|arr| arr.first().and_then(get_number))
        .is_some_and(|first| first > 0.5);

    let mut samples = Vec::with_capacity((width * height) as usize);
    match bits {
        1 => {
            let row_bytes = (width * components + 7) / 8;
            for row in 0..height {
                for col in 0..width {
                    let bit_index = col * components;
                    let byte = *data.get((row * row_bytes + bit_index / 8) as usize)?;
                    let bit = (byte >> (7 - (bit_index % 8))) & 1;
                    let mut on = bit == 1;
                    if inverted != is_mask {
                        on = !on;
                    }
                    samples.push(if on { 255 } else { 0 });
                }
            }
        }
        8 => {
            for row in 0..height {
                for col in 0..width {
                    let index = ((row * width + col) * components) as usize;
                    let value = *data.get(index)?;
                    samples.push(if inverted { 255 - value } else { value });
                }
            }
        }
        _ => return None,
    }

    Some(LumaGrid {
        width,
        height,
        samples,
    })
}

fn colorspace_components(doc: &Document, stream: &Stream) -> Option<i64> {
    let colorspace = stream.dict.get(b"ColorSpace").ok()?;
    let resolved = match colorspace {
        Object::Reference(id) => doc.get_object(*id).ok()?,
        other => other,
    };
    match resolved {
        Object::Name(name) => match name.as_slice() {
            b"DeviceGray" | b"CalGray" | b"G" => Some(1),
            b"DeviceRGB" | b"CalRGB" | b"RGB" => Some(3),
            b"DeviceCMYK" | b"CMYK" => Some(4),
            _ => None,
        },
        Object::Array(items) => match items.first().and_then(|o| o.as_name().ok()) {
            Some(b"ICCBased") => items
                .get(1)
                .and_then(|o| match o {
                    Object::Reference(id) => doc.get_object(*id).ok(),
                    other => Some(other),
                })
                .and_then(|o| o.as_stream().ok())
                .and_then(|icc| icc.dict.get(b"N").ok())
                .and_then(|n| n.as_i64().ok()),
            Some(b"Indexed") | Some(b"I") | Some(b"Separation") => Some(1),
            Some(b"DeviceN") => None,
            Some(b"CalGray") => Some(1),
            Some(b"CalRGB") | Some(b"Lab") => Some(3),
            _ => None,
        },
        _ => None,
    }
}

fn can_label_a_control(item: &TextItem) -> bool {
    let is_page_text = matches!(item.item_type, ItemType::Text);
    is_page_text && !item.text.trim().is_empty()
}

fn attach_labels(controls: &mut [FormControl], items: &[TextItem]) {
    if controls.is_empty() {
        return;
    }
    let mut by_page: HashMap<u32, Vec<&TextItem>> = HashMap::new();
    for item in items.iter().filter(|item| can_label_a_control(item)) {
        by_page.entry(item.page).or_default().push(item);
    }

    for control in controls.iter_mut() {
        let Some(page_items) = by_page.get(&control.page) else {
            continue;
        };
        control.label = nearest_label(control, page_items);
    }
}

fn nearest_label(control: &FormControl, page_items: &[&TextItem]) -> Option<String> {
    let control_center_y = control.y + control.height / 2.0;
    let control_right = control.x + control.width;
    let band = control.height / 2.0 + LABEL_BAND_TOLERANCE;

    let mut best_right: Option<(f32, &TextItem)> = None;
    let mut best_left: Option<(f32, &TextItem)> = None;

    for item in page_items {
        let item_center_y = item.y + item.height / 2.0;
        if (item_center_y - control_center_y).abs() > band {
            continue;
        }
        let gap_right = item.x - control_right;
        if (0.0..=LABEL_MAX_DISTANCE).contains(&gap_right) {
            if best_right.is_none_or(|(best, _)| gap_right < best) {
                best_right = Some((gap_right, item));
            }
            continue;
        }
        let gap_left = control.x - (item.x + item.width);
        if (0.0..=LABEL_MAX_DISTANCE).contains(&gap_left)
            && best_left.is_none_or(|(best, _)| gap_left < best)
        {
            best_left = Some((gap_left, item));
        }
    }

    best_right
        .or(best_left)
        .map(|(_, item)| item.text.trim().to_string())
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(rows: &[&str]) -> LumaGrid {
        let height = rows.len() as i64;
        let width = rows[0].len() as i64;
        let samples = rows
            .iter()
            .flat_map(|row| row.chars())
            .map(|c| match c {
                '#' => 0u8,
                '+' => 180u8,
                _ => 255u8,
            })
            .collect();
        LumaGrid {
            width,
            height,
            samples,
        }
    }

    fn classify(rows: &[&str]) -> Option<StampShape> {
        classify_luma_grid(&grid(rows))
    }

    #[test]
    fn empty_outlined_box_is_unchecked() {
        let shape = classify(&[
            "++++++++++",
            "+........+",
            "+........+",
            "+........+",
            "+........+",
            "+........+",
            "+........+",
            "+........+",
            "+........+",
            "++++++++++",
        ]);
        assert_eq!(shape, Some(StampShape::Unchecked));
    }

    #[test]
    fn borderless_check_mark_is_checked() {
        let shape = classify(&[
            "........+#",
            ".......+#+",
            "......+#+.",
            ".....+##..",
            "+#..+##+..",
            "###+##+...",
            ".#####....",
            "..###+....",
            "..+##.....",
            "...++.....",
        ]);
        assert_eq!(shape, Some(StampShape::Checked));
    }

    #[test]
    fn blank_image_is_not_a_control() {
        let shape = classify(&[
            "..........",
            "..........",
            "..........",
            "..........",
            "..........",
            "..........",
            "..........",
            "..........",
            "..........",
            "..........",
        ]);
        assert_eq!(shape, None);
    }

    #[test]
    fn stamp_size_gate_rejects_full_page_images() {
        assert!(is_stamp_sized(7.5, 7.5));
        assert!(!is_stamp_sized(612.0, 792.0));
        assert!(!is_stamp_sized(7.5, 60.0));
        assert!(!is_stamp_sized(2.0, 2.0));
    }
}
