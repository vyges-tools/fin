// SPDX-License-Identifier: Apache-2.0
//! Density fill — deciding where metal fill goes, separated from the database.
//!
//! Foundries require a minimum metal density per layer; the gaps between real routing are filled
//! with electrically inert shapes to reach it. This module decides where those shapes go, given
//! the area to fill and what already occupies it. It never touches a database.
//!
//! The whole algorithm is a loop over shape sizes, largest first: shrink the remaining area by
//! half the shape, bloat it back (which deletes anything too small to hold the shape and breaks
//! big regions into tractable pieces), tile the result on a fixed grid, keep only whole shapes,
//! and subtract what was placed before trying the next size down.
//!
//! # Provenance
//!
//! Rules **F1**…**F11**, reimplemented from the behaviour of OpenROAD's `DensityFill.cpp`.
//! Nothing is copied from it.
//!
//! The tests lean on properties that must hold of any correct fill — nothing overlaps the design,
//! spacing is respected, every shape is whole and a declared size — rather than on reproducing a
//! single reference file.

use std::collections::BTreeMap;

use vyges_loom::poly90::{Poly90Set, Rect};

/// One set of fill shapes and the spacings that govern them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShapeCfg {
    /// `(width, height)` to try, **in the order given** — largest first by convention (**F1**).
    pub shapes: Vec<(i32, i32)>,
    /// Minimum gap between two fill shapes.
    pub space_to_fill: i32,
    /// Minimum gap between fill and anything real.
    pub space_to_non_fill: i32,
    /// Extra gap along the routing direction only (**F3**). Zero for non-OPC fill.
    pub space_line_end: i32,
}

/// The rules for one layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayerCfg {
    pub space_to_outline: i32,
    /// Masks to cycle through; `<= 1` means the layer is single-mask and no mask is written.
    pub num_masks: u32,
    pub has_opc: bool,
    pub opc_halo: i32,
    pub opc: ShapeCfg,
    pub non_opc: ShapeCfg,
}

/// Fill rules, keyed by the layer name they apply to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    pub layers: BTreeMap<String, LayerCfg>,
}

/// One fill shape to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    pub rect: Rect,
    pub mask: u32,
    pub needs_opc: bool,
}

/// A µm value from the rules file, in DBU.
///
/// Truncating rather than rounding, matching upstream's implicit `double`→`int` conversion: a
/// spacing that came out a DBU larger than upstream's would be a different rule.
fn to_dbu(microns: f64, dbu: i32) -> i32 {
    (microns * dbu as f64) as i32
}

fn number(v: &serde_json::Value, key: &str) -> Result<f64, String> {
    v.get(key)
        .and_then(|x| x.as_f64())
        .ok_or_else(|| format!("`{key}` is missing or not a number"))
}

fn shapes_of(v: &serde_json::Value, dbu: i32) -> Result<Vec<(i32, i32)>, String> {
    let list = |k: &str| -> Result<Vec<f64>, String> {
        v.get(k)
            .and_then(|x| x.as_array())
            .ok_or_else(|| format!("`{k}` is missing or not a list"))?
            .iter()
            .map(|x| {
                x.as_f64()
                    .ok_or_else(|| format!("`{k}` holds a non-number"))
            })
            .collect()
    };
    let (w, h) = (list("width")?, list("height")?);
    // Zipped, so a mismatched pair is silently the shorter of the two — as upstream's transform
    // over two ranges also is.
    Ok(w.iter()
        .zip(h.iter())
        .map(|(a, b)| (to_dbu(*a, dbu), to_dbu(*b, dbu)))
        .collect())
}

fn shape_cfg(v: &serde_json::Value, dbu: i32, line_end: bool) -> Result<ShapeCfg, String> {
    Ok(ShapeCfg {
        shapes: shapes_of(v, dbu)?,
        space_to_fill: to_dbu(number(v, "space_to_fill")?, dbu),
        space_to_non_fill: to_dbu(number(v, "space_to_non_fill")?, dbu),
        // Only OPC fill states a line-end spacing, and even there it is optional.
        space_line_end: if line_end {
            v.get("space_line_end")
                .and_then(|x| x.as_f64())
                .map(|x| to_dbu(x, dbu))
                .unwrap_or(0)
        } else {
            0
        },
    })
}

/// Parse the rules file.
///
/// A `names` list applies one layer's rules to several layers; otherwise `name` names the single
/// layer. Layer names are resolved by the caller, which is the only party that knows the
/// technology — a rule naming a layer the technology lacks is that caller's error to report.
pub fn parse_rules(json: &str, dbu: i32) -> Result<Rules, String> {
    let root: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("rules file is not valid JSON: {e}"))?;
    let layers = root
        .get("layers")
        .and_then(|l| l.as_object())
        .ok_or("rules file has no `layers` object")?;

    let mut out = Rules::default();
    for (key, layer) in layers {
        let non_opc = layer
            .get("non-opc")
            .ok_or_else(|| format!("layer `{key}` has no `non-opc` section"))?;
        let opc = layer.get("opc");

        let cfg = LayerCfg {
            space_to_outline: to_dbu(number(layer, "space_to_outline")?, dbu),
            // A `datatype` LIST names one datatype per mask; a bare number is a single-mask layer.
            num_masks: non_opc
                .get("datatype")
                .and_then(|d| d.as_array())
                .map(|a| a.len() as u32)
                .unwrap_or(0),
            has_opc: opc.is_some(),
            opc_halo: match opc {
                Some(o) => to_dbu(number(o, "halo").unwrap_or(0.0), dbu),
                None => 0,
            },
            opc: match opc {
                Some(o) => shape_cfg(o, dbu, true)?,
                None => ShapeCfg::default(),
            },
            non_opc: shape_cfg(non_opc, dbu, false)?,
        };

        // `names` expands one rule over several layers; otherwise `name`, falling back to the key.
        let targets: Vec<String> = match layer.get("names").and_then(|n| n.as_array()) {
            Some(list) => list
                .iter()
                .filter_map(|n| n.as_str().map(String::from))
                .collect(),
            None => vec![layer
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or(key)
                .to_string()],
        };
        for t in targets {
            out.layers.insert(t, cfg.clone());
        }
    }
    Ok(out)
}

/// **F3** — the spacing between fill shapes, which is anisotropic.
///
/// The line-end spacing applies along the routing direction only: two fills end-to-end along a
/// horizontal layer need more room than two side by side.
pub fn spacing(is_horiz: bool, cfg: &ShapeCfg) -> (i32, i32) {
    let (mut sx, mut sy) = (cfg.space_to_fill, cfg.space_to_fill);
    if is_horiz {
        sx = sx.max(cfg.space_line_end);
    } else {
        sy = sy.max(cfg.space_line_end);
    }
    (sx, sy)
}

/// **F5** — remove the places where two separate regions are closer than the fill spacing.
///
/// Filling two nearly-touching regions independently can put two fills a sub-spacing distance
/// apart. Bloating each region *separately* and keeping only the overlaps the bloat introduced
/// finds exactly those places; subtracting them keeps the two fills apart.
///
/// Conservative on purpose: it may forbid a fill that would have been legal, which is the right
/// direction to be wrong in.
pub fn prune(area: &Poly90Set, space_x: i32, space_y: i32) -> Poly90Set {
    let pieces: Vec<Poly90Set> = area
        .polygons()
        .into_iter()
        .map(|p| {
            let mut s = Poly90Set::from_rects(&outline_rects(&p.outer));
            for h in &p.holes {
                s = s.difference(&Poly90Set::from_rects(&outline_rects(h)));
            }
            s.bloat(space_x, space_x, space_y, space_y)
        })
        .collect();

    let mut overlaps = Poly90Set::new();
    for i in 0..pieces.len() {
        for j in (i + 1)..pieces.len() {
            overlaps = overlaps.union(&pieces[i].intersection(&pieces[j]));
        }
    }
    area.difference(&overlaps)
}

/// A rectilinear outline as the rectangles it encloses.
///
/// Rebuilt through the slab decomposition rather than trusted as a polygon: `Poly90Set` is the
/// only thing that knows how to turn an outline back into area.
fn outline_rects(outline: &[vyges_loom::poly90::Point]) -> Vec<Rect> {
    // A rectilinear outline's bounding box, cut by every vertex coordinate, then tested for
    // membership — correct for any rectilinear shape, and these outlines are small.
    let (xs, ys): (Vec<i32>, Vec<i32>) = (
        {
            let mut v: Vec<i32> = outline.iter().map(|p| p.x).collect();
            v.sort_unstable();
            v.dedup();
            v
        },
        {
            let mut v: Vec<i32> = outline.iter().map(|p| p.y).collect();
            v.sort_unstable();
            v.dedup();
            v
        },
    );
    let mut out = Vec::new();
    for wx in xs.windows(2) {
        for wy in ys.windows(2) {
            let (cx, cy) = ((wx[0] + wx[1]) / 2, (wy[0] + wy[1]) / 2);
            if point_in_outline(outline, cx, cy) {
                out.push(Rect::new(wx[0], wy[0], wx[1], wy[1]));
            }
        }
    }
    out
}

fn point_in_outline(outline: &[vyges_loom::poly90::Point], x: i32, y: i32) -> bool {
    let n = outline.len();
    let mut inside = false;
    for i in 0..n {
        let (a, b) = (outline[i], outline[(i + 1) % n]);
        if (a.y > y) != (b.y > y) {
            let t = (y - a.y) as i64;
            let x_at = a.x as i64 + (b.x - a.x) as i64 * t / (b.y - a.y) as i64;
            if x_at > x as i64 {
                inside = !inside;
            }
        }
    }
    inside
}

/// **F1, F2, F6–F9** — the fill shapes for one area.
///
/// `mask_counter` runs across the whole layer, so the mask a shape gets depends on how many were
/// placed before it anywhere on that layer.
/// The verdict word for a run that placed `fills` shapes.
///
/// 🔑 **A pass word asserts that work was DONE; if none was, do not say it.** `filled` is what the
/// descriptor's assertion reads (`status == "filled"`), so a run that placed no shape at all would
/// otherwise pass a gate having changed nothing. Measured before this existed: a degenerate
/// `--area` returned `filled` with `fills: 0`, `layers_filled: 0` and exit 0, while the engine's
/// own event said *"density fill applied: 0 shape(s) over 0 layer(s)"*.
///
/// ⚠️ **`vacuous` is not an error.** Zero can be the right answer — a design already above every
/// density floor needs no fill. The caller reads the count and decides; what it must not do is
/// read a no-op as a completed transformation. A dry run keeps `planned`, which never claimed to
/// have filled anything and already fails the assertion.
pub fn settle_status(dry_run: bool, fills: usize) -> &'static str {
    if dry_run {
        return "planned";
    }
    if fills == 0 {
        return "vacuous";
    }
    "filled"
}

#[cfg(test)]
mod settle_status_tests {
    use super::settle_status;

    #[test]
    fn a_run_that_filled_nothing_is_not_reported_as_filled() {
        assert_eq!(settle_status(false, 0), "vacuous");
        assert_eq!(settle_status(false, 1), "filled");
    }

    #[test]
    fn a_dry_run_never_claims_to_have_filled_anything() {
        assert_eq!(settle_status(true, 0), "planned");
        assert_eq!(settle_status(true, 5), "planned");
    }
}

pub fn fill_area(
    area: &Poly90Set,
    is_horiz: bool,
    cfg: &ShapeCfg,
    num_masks: u32,
    needs_opc: bool,
    mask_counter: &mut u32,
) -> Vec<Fill> {
    let (space_x, space_y) = spacing(is_horiz, cfg);
    let masks = num_masks.max(1);
    let mut remaining = area.clone();
    let mut out = Vec::new();

    for &(w0, h0) in &cfg.shapes {
        // F2: the long side lies along the routing direction. A fill across the preferred
        // direction is a manufacturing problem, not a smaller fill.
        let (w, h) = if (is_horiz && w0 < h0) || (!is_horiz && h0 < w0) {
            (h0, w0)
        } else {
            (w0, h0)
        };
        if w <= 0 || h <= 0 {
            continue;
        }

        // F6: shrink then bloat by half the shape. This deletes anything too small to hold it and
        // breaks large regions into pieces small enough to tile.
        let (ew, ns) = (w / 2 - 1, h / 2 - 1);
        let pruned = prune(
            &remaining.shrink(ew, ew, ns, ns).bloat(ew, ew, ns, ns),
            space_x,
            space_y,
        );

        let mut placed = Poly90Set::new();
        for piece in pruned.polygons() {
            let sub = Poly90Set::from_rects(&outline_rects(&piece.outer));
            let sub = piece.holes.iter().fold(sub, |acc, hole| {
                acc.difference(&Poly90Set::from_rects(&outline_rects(hole)))
            });
            let Some(b) = sub.bounds() else { continue };

            // F7: a fixed grid from the bounding box. Upstream notes KLayout sweeps the origin
            // looking for maximum fill and it does not, so neither do we.
            let mut tiles = Vec::new();
            let mut x = b.x0;
            while x < b.x1 {
                let mut y = b.y0;
                while y < b.y1 {
                    tiles.push(Rect::new(x, y, x + w, y + h));
                    y += h + space_y;
                }
                x += w + space_x;
            }

            // F8: intersect with the area, then keep only shapes still the full size. A clipped
            // tile is the wrong size, and a wrong-sized fill is a DRC violation, not a small fill.
            let whole = Poly90Set::from_rects(&tiles)
                .intersection(&sub)
                .keep_sized(w, w, h, h);
            for r in whole.rects() {
                // F9: masks cycle; a single-mask layer writes no mask at all.
                let mask = if masks == 1 {
                    0
                } else {
                    *mask_counter % masks + 1
                };
                *mask_counter = mask_counter.wrapping_add(1);
                out.push(Fill {
                    rect: r,
                    mask,
                    needs_opc,
                });
            }
            placed = placed.union(&whole);
        }

        // Keep the next size down clear of what was just placed, by the fill-to-fill spacing.
        remaining = remaining.difference(&placed.bloat(space_x, space_x, space_y, space_y));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = r#"{
      "layers": {
        "met1": {
          "layer": 36, "name": "met1", "dir": "H", "datatype": 0,
          "space_to_outline": 0.07,
          "non-opc": {
            "datatype": 0,
            "width":  [2.00, 1.00, 0.58, 0.3],
            "height": [2.00, 1.00, 0.58, 0.3],
            "space_to_fill": 0.3,
            "space_to_non_fill": 3
          }
        },
        "grp": {
          "name": "met2", "names": ["met2", "met3"],
          "space_to_outline": 0.07,
          "non-opc": {
            "datatype": [0, 1], "width": [1.0], "height": [1.0],
            "space_to_fill": 0.2, "space_to_non_fill": 1
          },
          "opc": {
            "datatype": 0, "halo": 0.5, "width": [0.5], "height": [0.5],
            "space_to_fill": 0.1, "space_to_non_fill": 2, "space_line_end": 0.4
          }
        }
      }
    }"#;

    fn set(r: &[(i32, i32, i32, i32)]) -> Poly90Set {
        Poly90Set::from_rects(
            &r.iter()
                .map(|&(a, b, c, d)| Rect::new(a, b, c, d))
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn the_rules_file_becomes_database_units() {
        let r = parse_rules(RULES, 1000).expect("parses");
        let met1 = r.layers.get("met1").expect("met1");
        assert_eq!(met1.space_to_outline, 70, "0.07um at 1000 DBU/um");
        assert_eq!(met1.non_opc.space_to_fill, 300);
        assert_eq!(met1.non_opc.space_to_non_fill, 3000);
        assert_eq!(
            met1.non_opc.shapes,
            vec![(2000, 2000), (1000, 1000), (580, 580), (300, 300)],
            "largest first, in the order the file gives"
        );
        assert!(!met1.has_opc);
        assert_eq!(met1.num_masks, 0, "a bare datatype is a single-mask layer");
    }

    #[test]
    fn a_names_list_applies_one_rule_to_several_layers() {
        let r = parse_rules(RULES, 1000).expect("parses");
        assert!(r.layers.contains_key("met2") && r.layers.contains_key("met3"));
        assert_eq!(r.layers["met2"], r.layers["met3"]);
        // ...and the group key itself is not a layer.
        assert!(!r.layers.contains_key("grp"));
    }

    #[test]
    fn opc_rules_are_read_only_where_they_are_stated() {
        let r = parse_rules(RULES, 1000).expect("parses");
        let met2 = &r.layers["met2"];
        assert!(met2.has_opc);
        assert_eq!(met2.opc_halo, 500);
        assert_eq!(
            met2.opc.space_line_end, 400,
            "OPC may state a line-end spacing"
        );
        assert_eq!(met2.non_opc.space_line_end, 0, "non-OPC never does");
        assert_eq!(
            met2.num_masks, 2,
            "a datatype LIST names one datatype per mask"
        );
    }

    #[test]
    fn a_malformed_rules_file_is_refused_with_a_reason() {
        assert!(parse_rules("not json", 1000)
            .unwrap_err()
            .contains("valid JSON"));
        assert!(parse_rules("{}", 1000).unwrap_err().contains("`layers`"));
        let missing =
            r#"{"layers":{"m":{"space_to_outline":1,"non-opc":{"width":[1],"height":[1]}}}}"#;
        assert!(parse_rules(missing, 1000)
            .unwrap_err()
            .contains("space_to_fill"));
    }

    #[test]
    fn line_end_spacing_applies_along_the_routing_direction_only() {
        let cfg = ShapeCfg {
            space_to_fill: 100,
            space_line_end: 400,
            ..Default::default()
        };
        assert_eq!(
            spacing(true, &cfg),
            (400, 100),
            "horizontal: the x gap grows"
        );
        assert_eq!(
            spacing(false, &cfg),
            (100, 400),
            "vertical: the y gap grows"
        );
        let plain = ShapeCfg {
            space_to_fill: 100,
            ..Default::default()
        };
        assert_eq!(spacing(true, &plain), (100, 100));
    }

    #[test]
    fn the_long_side_of_a_shape_follows_the_routing_direction() {
        let cfg = ShapeCfg {
            shapes: vec![(100, 400)],
            space_to_fill: 0,
            ..Default::default()
        };
        let area = set(&[(0, 0, 10_000, 10_000)]);
        let mut c = 0;
        let horiz = fill_area(&area, true, &cfg, 1, false, &mut c);
        assert!(
            horiz
                .iter()
                .all(|f| f.rect.width() == 400 && f.rect.height() == 100),
            "a horizontal layer lays the shape on its side"
        );
        let mut c = 0;
        let vert = fill_area(&area, false, &cfg, 1, false, &mut c);
        assert!(vert
            .iter()
            .all(|f| f.rect.width() == 100 && f.rect.height() == 400));
    }

    #[test]
    fn every_fill_is_a_whole_shape_of_a_declared_size() {
        // The property that matters most: a clipped tile is the wrong size, and a wrong-sized
        // fill is a DRC violation rather than a smaller fill.
        let cfg = ShapeCfg {
            shapes: vec![(1000, 1000), (300, 300)],
            space_to_fill: 100,
            ..Default::default()
        };
        // A deliberately awkward area: not a multiple of any shape, with a notch.
        let area = set(&[(0, 0, 4321, 2345)]).difference(&set(&[(1000, 1000, 1500, 1500)]));
        let mut c = 0;
        let fills = fill_area(&area, true, &cfg, 1, false, &mut c);
        assert!(!fills.is_empty(), "an area this size can hold fill");
        for f in &fills {
            let (w, h) = (f.rect.width(), f.rect.height());
            assert!(
                cfg.shapes.contains(&(w, h)),
                "fill {w}x{h} is not a declared size: {:?}",
                cfg.shapes
            );
        }
    }

    #[test]
    fn fill_never_lands_outside_the_area_it_was_given() {
        let cfg = ShapeCfg {
            shapes: vec![(500, 500)],
            space_to_fill: 100,
            ..Default::default()
        };
        let area = set(&[(0, 0, 5000, 5000)]).difference(&set(&[(2000, 2000, 3000, 3000)]));
        let mut c = 0;
        for f in fill_area(&area, true, &cfg, 1, false, &mut c) {
            let corners = [
                (f.rect.x0, f.rect.y0),
                (f.rect.x1 - 1, f.rect.y0),
                (f.rect.x0, f.rect.y1 - 1),
                (f.rect.x1 - 1, f.rect.y1 - 1),
            ];
            for (x, y) in corners {
                assert!(
                    area.contains(x, y),
                    "fill {:?} reaches outside the area",
                    f.rect
                );
            }
        }
    }

    #[test]
    fn two_fills_are_never_closer_than_the_spacing() {
        let cfg = ShapeCfg {
            shapes: vec![(700, 700), (300, 300)],
            space_to_fill: 200,
            ..Default::default()
        };
        let area = set(&[(0, 0, 6000, 4000)]);
        let mut c = 0;
        let fills = fill_area(&area, true, &cfg, 1, false, &mut c);
        assert!(fills.len() > 4, "enough fills to be worth checking");
        for (i, a) in fills.iter().enumerate() {
            for b in &fills[i + 1..] {
                // Two rectangles are far enough apart if they are separated on either axis.
                let apart_x = a.rect.x1 + 200 <= b.rect.x0 || b.rect.x1 + 200 <= a.rect.x0;
                let apart_y = a.rect.y1 + 200 <= b.rect.y0 || b.rect.y1 + 200 <= a.rect.y0;
                assert!(
                    apart_x || apart_y,
                    "{:?} and {:?} are too close",
                    a.rect,
                    b.rect
                );
            }
        }
    }

    #[test]
    fn a_region_too_small_for_any_shape_gets_no_fill() {
        let cfg = ShapeCfg {
            shapes: vec![(1000, 1000)],
            space_to_fill: 100,
            ..Default::default()
        };
        let mut c = 0;
        assert!(fill_area(&set(&[(0, 0, 500, 500)]), true, &cfg, 1, false, &mut c).is_empty());
        assert!(fill_area(&Poly90Set::new(), true, &cfg, 1, false, &mut c).is_empty());
        assert_eq!(c, 0, "and nothing was numbered");
    }

    #[test]
    fn masks_cycle_and_a_single_mask_layer_writes_none() {
        let cfg = ShapeCfg {
            shapes: vec![(500, 500)],
            space_to_fill: 100,
            ..Default::default()
        };
        let area = set(&[(0, 0, 5000, 2000)]);

        let mut c = 0;
        let single = fill_area(&area, true, &cfg, 1, false, &mut c);
        assert!(
            single.iter().all(|f| f.mask == 0),
            "a single-mask layer writes no mask"
        );

        let mut c = 0;
        let triple = fill_area(&area, true, &cfg, 3, false, &mut c);
        assert!(triple.iter().all(|f| (1..=3).contains(&f.mask)));
        assert_eq!(triple[0].mask, 1);
        assert_eq!(triple[1].mask, 2);
        assert_eq!(triple[2].mask, 3);
        assert_eq!(triple[3].mask, 1, "and it wraps");
    }

    #[test]
    fn smaller_shapes_fill_what_the_larger_ones_could_not() {
        // The point of trying sizes in order: the offcuts a big shape leaves are worth filling.
        let big_only = ShapeCfg {
            shapes: vec![(1000, 1000)],
            space_to_fill: 100,
            ..Default::default()
        };
        let both = ShapeCfg {
            shapes: vec![(1000, 1000), (300, 300)],
            space_to_fill: 100,
            ..Default::default()
        };
        // A big region plus a detached pocket only the small shape can use. (Leftovers WITHIN a
        // big region are mostly narrower than the next size down, so a single region would not
        // show the effect — the sizes are for differently-sized gaps, not for packing one gap.)
        let area = set(&[(0, 0, 3400, 3400), (4000, 0, 4500, 500)]);
        let (mut c1, mut c2) = (0, 0);
        let a = fill_area(&area, true, &big_only, 1, false, &mut c1);
        let b = fill_area(&area, true, &both, 1, false, &mut c2);

        let covered = |f: &[Fill]| -> i64 {
            f.iter()
                .map(|x| x.rect.width() as i64 * x.rect.height() as i64)
                .sum()
        };
        assert!(
            covered(&b) > covered(&a),
            "the smaller shape reaches the pocket"
        );
        assert!(
            a.iter().all(|f| f.rect.x0 < 3400),
            "the big shape cannot fit the pocket"
        );
        assert!(b.iter().any(|f| f.rect.x0 >= 4000), "the small one can");
        // ...and the big shapes are still all there.
        assert_eq!(b.iter().filter(|f| f.rect.width() == 1000).count(), a.len());
    }

    #[test]
    fn pruning_keeps_separate_regions_from_being_filled_too_close() {
        // Two regions 100 apart with a fill spacing of 300: filling both independently would put
        // two fills 100 apart, so the near edges are excluded.
        let area = set(&[(0, 0, 1000, 1000), (1100, 0, 2100, 1000)]);
        let pruned = prune(&area, 300, 300);
        assert!(pruned.area() < area.area(), "the facing edges are given up");
        assert!(
            pruned.contains(10, 500) && pruned.contains(2000, 500),
            "the far sides survive"
        );
        assert!(
            !pruned.contains(1050, 500),
            "and the gap itself is not fillable anyway"
        );
    }
}
