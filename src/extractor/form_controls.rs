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

fn is_stamp_sized_image(stream: &Stream) -> bool {
    let is_image = stream
        .dict
        .get(b"Subtype")
        .ok()
        .and_then(|o| o.as_name().ok())
        .is_some_and(|name| name == b"Image");
    let side = |key: &[u8]| {
        stream
            .dict
            .get(key)
            .ok()
            .and_then(|o| o.as_i64().ok())
            .is_some_and(|value| (2..=MAX_STAMP_PIXELS).contains(&value))
    };
    is_image && side(b"Width") && side(b"Height")
}

fn stamp_controls(doc: &Document) -> Vec<FormControl> {
    let candidates: HashSet<ObjectId> = doc
        .objects
        .iter()
        .filter(|(_, object)| object.as_stream().is_ok_and(is_stamp_sized_image))
        .map(|(id, _)| *id)
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }

    let mut scan = StampScan {
        doc,
        candidates,
        classified: HashMap::new(),
        budget: MAX_CONTENT_OPERATIONS,
        page: 0,
    };
    let mut controls = Vec::new();
    for (page_num, page_id) in doc.get_pages() {
        controls.extend(scan.page_controls(page_num, page_id));
    }
    controls
}

struct StampScan<'a> {
    doc: &'a Document,
    candidates: HashSet<ObjectId>,
    classified: HashMap<ObjectId, Option<StampShape>>,
    budget: usize,
    page: u32,
}

impl StampScan<'_> {
    fn page_controls(&mut self, page_num: u32, page_id: ObjectId) -> Vec<FormControl> {
        let Ok(content) = self.doc.get_and_decode_page_content(page_id) else {
            return Vec::new();
        };
        self.page = page_num;
        self.budget = MAX_CONTENT_OPERATIONS;

        let mut found = Vec::new();
        let xobjects = page_xobjects(self.doc, page_id);
        let identity = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
        self.walk(&content.operations, &xobjects, &identity, 0, &mut found);

        if found.len() >= MIN_STAMPS_PER_PAGE {
            return found;
        }
        if !found.is_empty() {
            log::debug!(
                "page {page_num}: dropped {} checkbox stamp(s), fewer than the {MIN_STAMPS_PER_PAGE} needed to treat them as a form",
                found.len()
            );
        }
        Vec::new()
    }

    fn walk(
        &mut self,
        operations: &[Operation],
        xobjects: &HashMap<String, ObjectId>,
        base_ctm: &[f32; 6],
        depth: usize,
        found: &mut Vec<FormControl>,
    ) {
        let mut ctm = *base_ctm;
        let mut stack: Vec<[f32; 6]> = Vec::new();

        for op in operations {
            if self.budget == 0 {
                return;
            }
            self.budget -= 1;
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
                "Do" => self.enter_xobject(op, xobjects, &ctm, depth, found),
                _ => {}
            }
        }
    }

    fn enter_xobject(
        &mut self,
        op: &Operation,
        xobjects: &HashMap<String, ObjectId>,
        ctm: &[f32; 6],
        depth: usize,
        found: &mut Vec<FormControl>,
    ) {
        let Some(name) = op.operands.first().and_then(|o| o.as_name().ok()) else {
            return;
        };
        let Some(&id) = xobjects.get(String::from_utf8_lossy(name).as_ref()) else {
            return;
        };
        let Ok(stream) = self.doc.get_object(id).and_then(|o| o.as_stream()) else {
            return;
        };

        if self.candidates.contains(&id) {
            self.push_stamp(id, stream, ctm, found);
            return;
        }
        if depth < MAX_XOBJECT_DEPTH && is_form_xobject(stream) {
            self.enter_form(stream, ctm, depth, found);
        }
    }

    fn push_stamp(
        &mut self,
        id: ObjectId,
        stream: &Stream,
        ctm: &[f32; 6],
        found: &mut Vec<FormControl>,
    ) {
        let (x, y, width, height) = image_bbox_from_ctm(ctm);
        if !is_stamp_sized(width, height) {
            return;
        }
        let doc = self.doc;
        let shape = *self
            .classified
            .entry(id)
            .or_insert_with(|| classify_stamp_image(doc, stream));
        let Some(shape) = shape else { return };

        found.push(FormControl {
            name: String::new(),
            kind: FormControlKind::Checkbox,
            export_value: None,
            checked: shape == StampShape::Checked,
            label: None,
            tooltip: None,
            source: FormControlSource::StampImage,
            page: self.page,
            x,
            y,
            width,
            height,
        });
    }

    fn enter_form(
        &mut self,
        stream: &Stream,
        ctm: &[f32; 6],
        depth: usize,
        found: &mut Vec<FormControl>,
    ) {
        let bytes = stream
            .decompressed_content()
            .unwrap_or_else(|_| stream.content.clone());
        let Ok(content) = lopdf::content::Content::decode(&bytes) else {
            return;
        };
        let nested_ctm = multiply_matrices(&form_matrix(stream), ctm);
        let nested_xobjects = stream
            .dict
            .get(b"Resources")
            .ok()
            .and_then(|o| resolve_dict(self.doc, o))
            .map(|resources| xobject_ids(self.doc, resources))
            .unwrap_or_default();
        self.walk(
            &content.operations,
            &nested_xobjects,
            &nested_ctm,
            depth + 1,
            found,
        );
    }
}

fn is_form_xobject(stream: &Stream) -> bool {
    stream
        .dict
        .get(b"Subtype")
        .ok()
        .and_then(|o| o.as_name().ok())
        .is_some_and(|name| name == b"Form")
}

fn page_xobjects(doc: &Document, page_id: ObjectId) -> HashMap<String, ObjectId> {
    let Ok((page_dict, inherited)) = doc.get_page_resources(page_id) else {
        return HashMap::new();
    };
    let inherited_dicts = inherited
        .into_iter()
        .filter_map(|id| doc.get_dictionary(id).ok());
    page_dict
        .into_iter()
        .chain(inherited_dicts)
        .flat_map(|resources| xobject_ids(doc, resources))
        .collect()
}

fn xobject_ids(doc: &Document, resources: &Dictionary) -> HashMap<String, ObjectId> {
    let Some(xobjects) = resources
        .get(b"XObject")
        .ok()
        .and_then(|o| resolve_dict(doc, o))
    else {
        return HashMap::new();
    };
    xobjects
        .iter()
        .filter_map(|(name, value)| {
            Some((
                String::from_utf8_lossy(name).to_string(),
                value.as_reference().ok()?,
            ))
        })
        .collect()
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

    fn ink_ratio(&self, max_luma: u8, mut covers: impl FnMut(i64, i64) -> bool) -> Option<f32> {
        let mut total = 0usize;
        let mut inked = 0usize;
        for row in 0..self.height {
            for col in 0..self.width {
                if !covers(row, col) {
                    continue;
                }
                total += 1;
                if self.at(row, col) < max_luma {
                    inked += 1;
                }
            }
        }
        (total > 0).then(|| inked as f32 / total as f32)
    }

    fn inset(&self) -> (i64, i64) {
        let scale = |side: i64| ((side as f32 * INTERIOR_INSET_FRACTION).round() as i64).max(1);
        (scale(self.width), scale(self.height))
    }

    fn interior_ink(&self) -> Option<f32> {
        let (inset_x, inset_y) = self.inset();
        self.ink_ratio(STRONG_INK_LUMA, |row, col| {
            (inset_y..self.height - inset_y).contains(&row)
                && (inset_x..self.width - inset_x).contains(&col)
        })
    }

    fn ring_ink(&self, depth: i64, max_luma: u8) -> Option<f32> {
        if self.width - 2 * depth < 2 || self.height - 2 * depth < 2 {
            return None;
        }
        self.ink_ratio(max_luma, |row, col| {
            let inside = (depth..self.height - depth).contains(&row)
                && (depth..self.width - depth).contains(&col);
            let on_edge = row == depth
                || row == self.height - depth - 1
                || col == depth
                || col == self.width - depth - 1;
            inside && on_edge
        })
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
    let interior_ink = grid.interior_ink()?;
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
            Some(b"Indexed" | b"I" | b"Separation" | b"CalGray") => Some(1),
            Some(b"CalRGB" | b"Lab") => Some(3),
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
