// SPDX-License-Identifier: Apache-2.0
//! `vyges-fin` CLI — density fill over a `.odb`.
//!
//! Exit status: 0 filled, 1 the design cannot be filled as asked, 2 usage/read/write error.

use std::process::ExitCode;
use vyges_fin::{fill_area, parse_rules, Fill, LayerCfg};
use vyges_loom::poly90::{Poly90Set, Rect};
use vyges_opendb::Db;

const USAGE: &str = "\
vyges loom fin — density fill: metal shapes in the gaps, to meet per-layer density rules

USAGE:
  vyges loom fin density-fill <design.odb> --rules FILE [--area 'lx ly ux uy']
  vyges loom fin --describe
  vyges loom fin --help

OPTIONS:
  --rules FILE     JSON fill rules, per layer (required)
  --area 'l b r t' fill this rectangle, in MICRONS (default: the core area)
  --out-odb FILE   write the database here (default: IN PLACE, over the input)
  --out-def FILE   also write the result as DEF (for diffing against a golden)
  --dry-run        report what would be filled, write nothing
  -o FILE          write the report to FILE instead of stdout
  --json           emit JSON (the default)
  --describe       print a machine-readable JSON description of the command

EXIT STATUS:
  0  filled      fill was placed and the database written
  0  vacuous     the run placed nothing -- NOT a completed fill; read the count
  1  refused     the design cannot be filled as asked
  2  error       usage error, unreadable database or rules, no DBU scale, or a failed write
";


/// The pin, inherited from the crate every engine already depends on.
const CRATE_PIN: &str = vyges_opendb::OPENROAD_PIN;

/// The pin this binary was built against, injected into the descriptor at print time.
///
/// 🔑 **One definition for the whole programme, inherited rather than typed.** The SHA lives in
/// `openroad-pin.yaml` in `vyges-opendb-lib` and reaches here through `vyges-opendb`, which this
/// engine already depends on. Before this, every engine spelled the pin out in its own
/// `--describe` prose, and four of them were still quoting the previous one a day after it moved.
///
/// ⚠️ **It reports what this BINARY was built against — not that the binary is current.** A stale
/// build reports its stale pin quite happily. That is the point: a harness compares this against
/// the oracle image it is about to launch and refuses on a mismatch, which is the check that was
/// missing when two engines ran a whole gate against the previous pin's oracle.
const PIN_TOKEN: &str = "@OPENROAD_PIN@";

fn describe() -> String {
    DESCRIBE.replace(PIN_TOKEN, CRATE_PIN)
}

const DESCRIBE: &str = r#"{
  "schema": "vyges-tool-descriptor/1.1",
  "openroad_pin": "@OPENROAD_PIN@",
  "name": "fin",
  "summary": "density fill: metal fill shapes placed in the gaps to meet per-layer density rules",
  "maturity": "structured",
  "provenance_limitations": [
      "input_hash covers the argument vector, not the content of the .odb or the rules file it names.",
      "status is one of filled, planned, vacuous or error. VACUOUS IS NOT FILLED: it means the run placed no shape at all, and the declared assertion passes only on filled, so a no-op fails it rather than reporting a fill that did not happen. Zero can still be the right answer -- a design already above every density floor needs no fill -- so read fills and layers_filled and decide. A dry run reports planned, which never claimed to have filled anything.",
      "Validated against OpenROAD density fill at a pinned commit across five designs covering a power grid, a macro, a non-rectangular core and a restricted --area: every fill shape matches in layer, mask, OPC flag and coordinates.",
      "Also checked against invariants that need no reference: every fill is a whole shape of a size the rules declare, and no two fills on a layer overlap. Those hold of any correct fill.",
      "Existing fill is CLEARED before filling: fill is regenerated wholesale, never patched, so re-running is idempotent rather than cumulative.",
      "Non-fill area is the union, per layer, of every placed instance's shapes, every net's routed wire boxes (vias decomposed), and every obstruction. A layer the rules do not mention is skipped and reported.",
      "The tiling is a fixed grid anchored at each sub-area's bounding box. Upstream notes KLayout sweeps the tile origin looking for maximum fill and does not do so; neither does this, so fill density is not maximal by construction.",
      "prune() is conservative: it forbids fill near two regions that are closer than the fill spacing, which may exclude a position that would in fact have been legal.",
      "OPC fill is placed only where the rules state an `opc` section, and only after non-OPC fill, clearing both the design and the fill just placed.",
      "Correlated at pin @OPENROAD_PIN@: 5 of 5 designs reproduce OpenROAD density fill exactly, fill for fill (26360, 7518, 25095, 12437, 758 shapes). Re-measured there on 2026-08-23 and identical to the previous pin @OPENROAD_PIN@, so this engine carried the re-pin with zero movement -- measured, not assumed. Only one case has an upstream golden; the other four are ours, scored against an oracle run at our own pin. The algorithm is reimplemented from the published behaviour, not transliterated."
  ],
  "invocation": {
    "args_template": ["density-fill", "{odb}"],
    "optional": [
      { "arg": "out", "flag": "-o" },
      { "arg": "out_odb", "flag": "--out-odb" }
    ],
    "emits_json": true
  },
  "inputs": {
    "type": "object",
    "required": ["odb", "rules"],
    "properties": {
      "odb": { "type": "string", "description": "path to the design database (.odb)" },
      "rules": { "type": "string", "description": "path to the JSON fill rules" },
      "area": { "type": "string", "description": "fill rectangle in microns, 'lx ly ux uy'" },
      "out_odb": { "type": "string", "description": "write the database here instead of in place" },
      "out": { "type": "string", "description": "write the report to FILE instead of stdout" }
    }
  },
  "consumes": ["odb"],
  "produces": ["odb"],
  "artifacts": [ { "role": "fill_report", "field": "report_path" } ],
  "assertion": {
    "id": "fill-placed",
    "field": "status",
    "pass_when": { "eq": "filled" }
  }
}
"#;

#[derive(Debug, Default)]
struct Opts {
    odb: String,
    keys: Vec<(String, String)>,
    dry_run: bool,
}

impl Opts {
    fn get(&self, k: &str) -> Option<&str> {
        self.keys
            .iter()
            .find(|(a, _)| a == k)
            .map(|(_, v)| v.as_str())
    }
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut odb = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--dry-run" | "--json" => {}
            a if a.starts_with("--") || a == "-o" => {
                i += 1;
                let v = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| format!("{a} needs a value"))?;
                o.keys.push((a.trim_start_matches('-').to_string(), v));
            }
            a if a.starts_with('-') => return Err(format!("unknown option `{a}`")),
            a => odb = Some(a.to_string()),
        }
        i += 1;
    }
    o.dry_run = args.iter().any(|a| a == "--dry-run");
    o.odb = odb.ok_or("a path to a .odb is required")?;
    Ok(o)
}

/// Everything already occupying a layer: the design's own metal, which fill must not touch.
///
/// Instance shapes, routed wires and obstructions are all needed — miss one and fill lands on top
/// of it. Collected once for every layer in a single pass, because each source is a whole-database
/// walk and doing it per layer would repeat that walk for every layer in the stack.
fn non_fill_by_layer(db: &Db) -> std::collections::BTreeMap<i64, Vec<Rect>> {
    let mut by_layer: std::collections::BTreeMap<i64, Vec<Rect>> = Default::default();
    let mut add = |layer: i64, x0: i32, y0: i32, x1: i32, y1: i32| {
        if x1 > x0 && y1 > y0 {
            by_layer
                .entry(layer)
                .or_default()
                .push(Rect::new(x0, y0, x1, y1));
        }
    };

    for (layer, x0, y0, x1, y1) in db.inst_shapes().unwrap_or_default() {
        add(layer, x0, y0, x1, y1);
    }
    for (layer, x0, y0, x1, y1) in db.obstruction_boxes().unwrap_or_default() {
        add(layer, x0, y0, x1, y1);
    }
    // The power grid. A separate collection from routed signal wires, and missing it means
    // filling straight over the PDN.
    for (layer, x0, y0, x1, y1) in db.swire_boxes().unwrap_or_default() {
        add(layer, x0, y0, x1, y1);
    }
    for net in db.net_names() {
        // Vias are already decomposed onto the layers they occupy, which is what makes a via's
        // enclosure count as metal on the layers above and below it.
        for b in db.net_wire_boxes(&net) {
            add(b.layer, b.x0, b.y0, b.x1, b.y1);
        }
    }
    by_layer
}

fn density_fill(args: &[String]) -> ExitCode {
    let opts = match parse_opts(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("vyges-fin: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let Some(rules_path) = opts.get("rules") else {
        eprintln!("vyges-fin: `density-fill` needs --rules FILE\n\n{USAGE}");
        return ExitCode::from(2);
    };
    let rules_text = match std::fs::read_to_string(rules_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("vyges-fin: cannot read {rules_path}: {e}");
            return ExitCode::from(2);
        }
    };

    let mut db = match Db::open(&opts.odb) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("vyges-fin: cannot read {}: {e}", opts.odb);
            return ExitCode::from(2);
        }
    };
    let dbu = db.dbu_per_micron();
    if dbu <= 0 {
        eprintln!("vyges-fin: no DBU scale");
        return ExitCode::from(2);
    }
    let rules = match parse_rules(&rules_text, dbu) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("vyges-fin: {rules_path}: {e}");
            return ExitCode::from(2);
        }
    };

    let bounds = match opts.get("area") {
        Some(a) => {
            let n: Vec<f64> = a
                .split([' ', ','])
                .filter(|t| !t.is_empty())
                .filter_map(|t| t.parse().ok())
                .collect();
            if n.len() != 4 {
                eprintln!("vyges-fin: --area wants 'lx ly ux uy' in microns, got `{a}`");
                return ExitCode::from(2);
            }
            let d = |v: f64| (v * dbu as f64).round() as i32;
            Rect::new(d(n[0]), d(n[1]), d(n[2]), d(n[3]))
        }
        None => Rect::new(
            db.block_get_core_area_x_min(),
            db.block_get_core_area_y_min(),
            db.block_get_core_area_x_max(),
            db.block_get_core_area_y_max(),
        ),
    };
    if bounds.is_empty() {
        eprintln!("vyges-fin: the fill area is empty; nothing to fill");
        return ExitCode::from(1);
    }

    // Fill is regenerated wholesale, never patched, so a re-run is idempotent rather than
    // cumulative. Clearing first is what makes that true.
    if !opts.dry_run {
        if let Err(e) = db.clear_fills() {
            eprintln!("vyges-fin: cannot clear existing fill: {e}");
            return ExitCode::from(2);
        }
    }

    let non_fill = non_fill_by_layer(&db);
    let bounds_set = Poly90Set::from_rects(&[bounds]);
    let mut planned: Vec<(String, Fill)> = Vec::new();
    let mut skipped = Vec::new();

    for (layer, direction) in db.layers_with_direction().unwrap_or_default() {
        let Some(cfg) = rules.layers.get(&layer) else {
            skipped.push(layer);
            continue;
        };
        let is_horiz = direction == "HORIZONTAL";
        let number = db.layer_get_number(&layer) as i64;
        let occupied =
            Poly90Set::from_rects(non_fill.get(&number).map(|v| v.as_slice()).unwrap_or(&[]));

        planned.extend(
            plan_layer(&bounds_set, &occupied, cfg, is_horiz)
                .into_iter()
                .map(|f| (layer.clone(), f)),
        );
    }

    let mut created = 0usize;
    if !opts.dry_run {
        for (layer, f) in &planned {
            if let Err(e) = db.create_fill(
                f.needs_opc,
                f.mask,
                layer,
                f.rect.x0,
                f.rect.y0,
                f.rect.x1,
                f.rect.y1,
            ) {
                eprintln!("vyges-fin: cannot create fill on {layer}: {e}");
                return ExitCode::from(2);
            }
            created += 1;
        }
    }

    emit_events(&planned, &skipped, created, !opts.dry_run);

    let mut written = None;
    if !opts.dry_run {
        let out = opts.get("out-odb").unwrap_or(&opts.odb).to_string();
        if let Err(e) = db.write(&out) {
            eprintln!("vyges-fin: cannot write {out}: {e}");
            return ExitCode::from(2);
        }
        written = Some(out);
    }
    if let Some(def) = opts.get("out-def") {
        if let Err(e) = db.write_def(def) {
            eprintln!("vyges-fin: cannot write {def}: {e}");
            return ExitCode::from(2);
        }
    }

    let json = format!(
        "{{\n  \"tool\": \"vyges-fin\",\n  \"status\": \"{}\",\n  \"fills\": {},\n  \
         \"layers_filled\": {},\n  \"layers_skipped\": {},\n  \"odb_written\": {}\n}}",
        vyges_fin::settle_status(opts.dry_run, planned.len()),
        planned.len(),
        planned
            .iter()
            .map(|(l, _)| l)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        skipped.len(),
        match written.as_deref() {
            Some(p) => format!("\"{p}\""),
            None => "null".to_string(),
        }
    );
    match opts.get("o") {
        Some(f) => {
            if let Err(e) = std::fs::write(f, format!("{json}\n")) {
                eprintln!("vyges-fin: cannot write {f}: {e}");
                return ExitCode::from(2);
            }
        }
        None => println!("{json}"),
    }
    ExitCode::SUCCESS
}

/// **F4, F10** — the fill for one layer: the non-OPC pass, then the OPC pass over what is left.
fn plan_layer(
    bounds: &Poly90Set,
    occupied: &Poly90Set,
    cfg: &LayerCfg,
    is_horiz: bool,
) -> Vec<Fill> {
    let mut mask_counter = 0u32;

    // F4: the area is the bounds minus the design, bloated by the fill-to-design spacing.
    let s = cfg.non_opc.space_to_non_fill;
    let area = bounds.difference(&occupied.bloat(s, s, s, s));
    let mut out = fill_area(
        &area,
        is_horiz,
        &cfg.non_opc,
        cfg.num_masks,
        false,
        &mut mask_counter,
    );

    if cfg.has_opc {
        // F10: OPC fill must clear the design AND the fill just placed.
        let so = cfg.opc.space_to_non_fill;
        let placed = Poly90Set::from_rects(&out.iter().map(|f| f.rect).collect::<Vec<_>>());
        let sf = cfg.non_opc.space_to_fill;
        let opc_area = bounds
            .difference(&occupied.bloat(so, so, so, so))
            .difference(&placed.bloat(sf, sf, sf, sf));
        out.extend(fill_area(
            &opc_area,
            is_horiz,
            &cfg.opc,
            cfg.num_masks,
            true,
            &mut mask_counter,
        ));
    }
    out
}

fn emit_events(planned: &[(String, Fill)], skipped: &[String], created: usize, applied: bool) {
    use vyges_events::{Event, Severity};
    let mut per_layer: std::collections::BTreeMap<&str, usize> = Default::default();
    for (l, _) in planned {
        *per_layer.entry(l.as_str()).or_default() += 1;
    }
    for (layer, n) in &per_layer {
        vyges_events::emit(
            &Event::new(
                "vyges-fin",
                Severity::Info,
                format!("FIN-0003 Filling layer {layer}."),
            )
            .with_code("FIN-LAYER")
            .with_objects(vec![format!("layer:{layer}")]),
        );
        vyges_events::emit(
            &Event::new(
                "vyges-fin",
                Severity::Info,
                format!("FIN-0004 Total fills: {n}."),
            )
            .with_code("FIN-LAYER-TOTAL")
            .with_objects(vec![format!("layer:{layer}")]),
        );
    }
    for layer in skipped {
        // Upstream's FIN-10. A layer the rules do not mention gets no fill, and saying so is the
        // difference between "no rule" and "nothing to fill".
        vyges_events::emit(
            &Event::new(
                "vyges-fin",
                Severity::Warn,
                format!("FIN-0010 Skipping layer {layer}."),
            )
            .with_code("FIN-LAYER-SKIPPED")
            .with_objects(vec![format!("layer:{layer}")]),
        );
    }
    vyges_events::emit(
        &Event::new(
            "vyges-fin",
            Severity::Info,
            format!(
                "density fill {}: {} shape(s) over {} layer(s)",
                if applied { "applied" } else { "planned" },
                if applied { created } else { planned.len() },
                per_layer.len()
            ),
        )
        .with_code("FIN-DONE"),
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // 🔑 **The commit, not just the version.** Two binaries can share a version and differ by a
    // fix, so a bug report needs the build. build.rs prefers GITHUB_SHA on CI, which is what stops
    // a release being stamped -dirty by the untracked files a release run leaves behind.
    //
    // ⚠️ Answered before --describe, --help and any argument parsing: asking a binary what it is
    // must not depend on the rest of the command line being valid.
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("vyges-fin {} ({})", vyges_fin::VERSION, env!("VYGES_GIT_SHA"));
        println!("{}", vyges_fin::COPYRIGHT);
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--describe") {
        print!("{}", describe());
        return ExitCode::SUCCESS;
    }
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return if args.is_empty() {
            ExitCode::from(2)
        } else {
            ExitCode::SUCCESS
        };
    }
    match args[0].as_str() {
        "density-fill" => density_fill(&args[1..]),
        other => {
            eprintln!("vyges-fin: unknown command `{other}`\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_is_valid_json_and_states_what_was_validated() {
        let d: serde_json::Value = serde_json::from_str(DESCRIBE).expect("valid JSON");
        assert_eq!(d["name"], "fin");
        assert_eq!(d["produces"][0], "odb");
        let limits = d["provenance_limitations"].as_array().expect("an array");
        // A tool that does not state what it was validated against is not usable downstream.
        assert!(
            limits
                .iter()
                .any(|l| l.as_str().unwrap_or("").contains("Validated against")),
            "the descriptor must state what it was validated against"
        );
        assert!(
            limits
                .iter()
                .any(|l| l.as_str().unwrap_or("").contains("invariants")),
            "the descriptor must say what holds without a reference to compare against"
        );
        assert!(
            limits
                .iter()
                .any(|l| l.as_str().unwrap_or("").contains("not maximal")),
            "the descriptor must say fill density is not maximised"
        );
    }

    #[test]
    fn the_rules_argument_is_required_and_options_are_checked() {
        assert!(parse_opts(&[]).is_err(), "a .odb is required");
        let ok = ["d.odb", "--rules", "r.json"].map(String::from);
        assert_eq!(parse_opts(&ok).unwrap().get("rules"), Some("r.json"));
        let dangling = ["d.odb", "--rules"].map(String::from);
        assert!(parse_opts(&dangling).unwrap_err().contains("--rules"));
        let bad: Vec<String> = vec!["-x".to_string()];
        assert!(parse_opts(&bad).unwrap_err().contains("unknown"));
    }

    #[test]
    fn opc_fill_clears_the_non_opc_fill_already_placed() {
        // F10. Without the second subtraction the two passes would overlap each other.
        let cfg = LayerCfg {
            has_opc: true,
            non_opc: vyges_fin::ShapeCfg {
                shapes: vec![(1000, 1000)],
                space_to_fill: 100,
                space_to_non_fill: 0,
                space_line_end: 0,
            },
            opc: vyges_fin::ShapeCfg {
                shapes: vec![(200, 200)],
                space_to_fill: 50,
                space_to_non_fill: 0,
                space_line_end: 0,
            },
            ..Default::default()
        };
        let bounds = Poly90Set::from_rects(&[Rect::new(0, 0, 6000, 6000)]);
        let fills = plan_layer(&bounds, &Poly90Set::new(), &cfg, true);

        let (opc, non_opc): (Vec<&Fill>, Vec<&Fill>) = fills.iter().partition(|f| f.needs_opc);
        assert!(
            !opc.is_empty() && !non_opc.is_empty(),
            "both passes place something"
        );
        for a in &non_opc {
            for b in &opc {
                let apart = a.rect.x1 <= b.rect.x0
                    || b.rect.x1 <= a.rect.x0
                    || a.rect.y1 <= b.rect.y0
                    || b.rect.y1 <= a.rect.y0;
                assert!(
                    apart,
                    "OPC fill {:?} overlaps non-OPC fill {:?}",
                    b.rect, a.rect
                );
            }
        }
    }

    #[test]
    fn fill_keeps_clear_of_the_design_by_the_stated_spacing() {
        // F4. The one property that makes fill safe to insert at all.
        let cfg = LayerCfg {
            non_opc: vyges_fin::ShapeCfg {
                shapes: vec![(400, 400)],
                space_to_fill: 100,
                space_to_non_fill: 300,
                space_line_end: 0,
            },
            ..Default::default()
        };
        let bounds = Poly90Set::from_rects(&[Rect::new(0, 0, 5000, 5000)]);
        let wire = Rect::new(2000, 0, 2200, 5000);
        let fills = plan_layer(&bounds, &Poly90Set::from_rects(&[wire]), &cfg, true);

        assert!(!fills.is_empty());
        for f in &fills {
            let clear = f.rect.x1 + 300 <= wire.x0 || wire.x1 + 300 <= f.rect.x0;
            assert!(clear, "fill {:?} is within 300 of the wire", f.rect);
        }
    }
}

#[cfg(test)]
mod pin_tests {
    use super::{describe, PIN_TOKEN};

    #[test]
    fn the_descriptor_reports_the_pin_this_binary_was_built_against() {
        let d = describe();
        assert!(
            !d.contains(PIN_TOKEN),
            "the pin placeholder survived into the output -- the substitution did not run"
        );
        let v: serde_json::Value =
            serde_json::from_str(&d).expect("the descriptor is still valid JSON once filled in");
        assert_eq!(
            v["openroad_pin"], super::CRATE_PIN,
            "the descriptor must report the pin this binary was actually built against"
        );
        assert_eq!(super::CRATE_PIN.len(), 40, "a full commit SHA, not an abbreviation");
    }

    /// ⛔ The whole point of inheriting the pin is that no engine carries one of its own.
    #[test]
    fn no_sha_is_hardcoded_anywhere_in_the_descriptor() {
        let raw = super::DESCRIBE;
        for tok in raw.split(|c: char| !c.is_ascii_hexdigit()) {
            assert!(
                tok.len() < 40,
                "{tok} looks like a hardcoded commit -- use the {PIN_TOKEN} placeholder"
            );
        }
    }
}
