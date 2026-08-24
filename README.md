# vyges-fin

Density fill over the OpenDB design database: metal shapes placed in the gaps between real routing,
so each layer meets the foundry's minimum metal density.

```text
vyges physical fin density-fill design.odb --rules fill.json
```

## Correctness

Validated against OpenROAD's density fill at a pinned commit, across five designs chosen for the
geometry they exercise — a power grid, a macro, a non-rectangular core, a near-empty floorplan, and
a restricted `--area`. Every fill shape matches in layer, mask, OPC flag and coordinates.

Each design is additionally checked against invariants that need no reference at all: every fill is
a whole shape of a declared size, and no two fills on a layer overlap. Those hold of any correct
fill.

## What it does

For each layer the rules mention, in order of the shape sizes given:

1. take the fill area (the core, or `--area`) minus the design's own metal, bloated by the
   fill-to-design spacing;
2. **shrink then bloat** by half the shape — which deletes anything too small to hold it and
   breaks big regions into pieces small enough to tile;
3. tile a fixed grid over each piece, keep only **whole** shapes, and place them;
4. subtract what was placed and try the next size down.

OPC fill, where a layer declares it, is a second pass that must also clear the fill just placed.

Fill is **regenerated wholesale, never patched**: existing fill is cleared first, so re-running is
idempotent rather than cumulative.

## Details that decide the output

- **The long side follows the routing direction.** A fill shape lying across the preferred
  direction is a manufacturing problem, not a smaller fill.
- **Spacing is anisotropic**: the line-end spacing applies along the routing direction only.
- **Only whole shapes are placed.** A tile clipped by the area it was tiled into is the wrong size,
  and a wrong-sized fill is a DRC violation rather than a smaller fill.
- **Masks cycle**; a single-mask layer writes no mask at all.
- A layer the rules do not mention is **skipped and reported** — the difference between "no rule"
  and "nothing to fill".

## What it does not do

- **The tiling is a fixed grid** anchored at each piece's bounding box. Upstream notes that KLayout
  sweeps the tile origin looking for maximum fill and does not do so; neither does this. **Fill
  density is not maximal by construction.**
- `prune` is conservative: it forbids fill where two regions are closer than the fill spacing, which
  may exclude a position that would in fact have been legal.

## Exit status

| | |
| --- | --- |
| `0` | filled — fill was placed and the database written |
| `1` | refused — the design cannot be filled as asked (an empty fill area) |
| `2` | error — usage, unreadable database or rules, no DBU scale, or a failed write |

## Building

```text
cargo build --release
cargo test
```

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
