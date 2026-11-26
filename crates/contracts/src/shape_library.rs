//! Ready-made vector shapes for diagrams beyond rectangles and ellipses: the shapes paper figures use for encoders and
//! decoders (trapezoids), data and latents (parallelograms), stores and caches (cylinders), operators (circled plus and
//! times), decisions (diamonds), tensors (cuboids) and flow (block arrows).
//!
//! Each recipe is a Canvas `PathSpec` in a 1000 x 1000 view box. A Path node stretches its view box to its bounds, so one
//! recipe serves every size: set the node's bounds to the box you want, and its fill and stroke as for a rectangle. A Path
//! holds no text: place a Text node over it (same bounds, centred, vertical_align middle). Nothing here is serialized into a
//! document by itself; the MCP server lists the recipes (`designllm://shapes`) and the desktop may insert them.

use crate::{FillRule, PathCommand, PathSpec, Rect};

/// Side of the square view box of every recipe.
pub const SHAPE_VIEW_BOX: i64 = 1000;

/// One named shape.
#[derive(Debug, Clone, Copy)]
pub struct ShapeRecipe {
    pub id: &'static str,
    /// What the shape stands for in a figure, and how to orient it.
    pub use_for: &'static str,
    pub commands: &'static [PathCommand],
}

impl ShapeRecipe {
    /// The recipe as a Canvas path (view box 0 0 1000 1000, nonzero fill).
    pub fn path(&self) -> PathSpec {
        PathSpec {
            view_box: Rect { x: 0, y: 0, width: SHAPE_VIEW_BOX, height: SHAPE_VIEW_BOX },
            fill_rule: FillRule::Nonzero,
            commands: self.commands.to_vec(),
        }
    }
}

use PathCommand::{Close, CubicTo, LineTo, MoveTo};

/// Cubic Bezier circle constant (4/3 * (sqrt 2 - 1)) times the 400 radius of the circled operators.
const K400: i64 = 221;
/// The same for a 500 radius.
const K500: i64 = 276;

/// A circle of radius 400 centred in the view box, clockwise from the top, then the operator's two strokes.
macro_rules! circled {
    ($($mark:expr),* $(,)?) => {
        [
            MoveTo { x: 500, y: 100 },
            CubicTo { x1: 500 + K400, y1: 100, x2: 900, y2: 500 - K400, x: 900, y: 500 },
            CubicTo { x1: 900, y1: 500 + K400, x2: 500 + K400, y2: 900, x: 500, y: 900 },
            CubicTo { x1: 500 - K400, y1: 900, x2: 100, y2: 500 + K400, x: 100, y: 500 },
            CubicTo { x1: 100, y1: 500 - K400, x2: 500 - K400, y2: 100, x: 500, y: 100 },
            Close,
            $($mark),*
        ]
    };
}

const CIRCLE_PLUS: [PathCommand; 10] =
    circled![MoveTo { x: 500, y: 100 }, LineTo { x: 500, y: 900 }, MoveTo { x: 100, y: 500 }, LineTo { x: 900, y: 500 }];
/// The diagonals of the 400 circle meet it at 500 +- 283.
const CIRCLE_TIMES: [PathCommand; 10] =
    circled![MoveTo { x: 217, y: 217 }, LineTo { x: 783, y: 783 }, MoveTo { x: 783, y: 217 }, LineTo { x: 217, y: 783 }];

/// Every recipe, in listing order.
pub const SHAPES: &[ShapeRecipe] = &[
    ShapeRecipe {
        id: "trapezoid_narrowing_right",
        use_for: "Encoder in a left-to-right figure (wide input side on the left, compressed side on the right), e.g. a VAE or U-Net encoder, a projection that reduces dimension.",
        commands: &[MoveTo { x: 0, y: 0 }, LineTo { x: 1000, y: 200 }, LineTo { x: 1000, y: 800 }, LineTo { x: 0, y: 1000 }, Close],
    },
    ShapeRecipe {
        id: "trapezoid_widening_right",
        use_for: "Decoder in a left-to-right figure (narrow latent side on the left, wide output side on the right), an up-projection.",
        commands: &[MoveTo { x: 0, y: 200 }, LineTo { x: 1000, y: 0 }, LineTo { x: 1000, y: 1000 }, LineTo { x: 0, y: 800 }, Close],
    },
    ShapeRecipe {
        id: "trapezoid_narrowing_up",
        use_for: "Encoder in a bottom-to-top figure (wide at the bottom where the input enters, narrow at the top).",
        commands: &[MoveTo { x: 200, y: 0 }, LineTo { x: 800, y: 0 }, LineTo { x: 1000, y: 1000 }, LineTo { x: 0, y: 1000 }, Close],
    },
    ShapeRecipe {
        id: "trapezoid_widening_up",
        use_for: "Decoder in a bottom-to-top figure (narrow at the bottom, wide at the top where the output leaves).",
        commands: &[MoveTo { x: 0, y: 0 }, LineTo { x: 1000, y: 0 }, LineTo { x: 800, y: 1000 }, LineTo { x: 200, y: 1000 }, Close],
    },
    ShapeRecipe {
        id: "parallelogram",
        use_for: "Data, input/output or a latent variable (z, x, y), the flowchart data symbol.",
        commands: &[MoveTo { x: 200, y: 0 }, LineTo { x: 1000, y: 0 }, LineTo { x: 800, y: 1000 }, LineTo { x: 0, y: 1000 }, Close],
    },
    ShapeRecipe {
        id: "cylinder",
        use_for: "A store: database, memory bank, KV cache, codebook, retrieval index. Keep it upright; the top rim is drawn by the stroke.",
        // The rim runs the same way round as the outline: the nonzero fill closes it, and an opposite rim would cancel the
        // outline's winding and leave the band under the rim unpainted.
        commands: &[
            MoveTo { x: 0, y: 120 },
            CubicTo { x1: 0, y1: 54, x2: 224, y2: 0, x: 500, y: 0 },
            CubicTo { x1: 776, y1: 0, x2: 1000, y2: 54, x: 1000, y: 120 },
            LineTo { x: 1000, y: 880 },
            CubicTo { x1: 1000, y1: 946, x2: 776, y2: 1000, x: 500, y: 1000 },
            CubicTo { x1: 224, y1: 1000, x2: 0, y2: 946, x: 0, y: 880 },
            Close,
            MoveTo { x: 1000, y: 120 },
            CubicTo { x1: 1000, y1: 186, x2: 776, y2: 240, x: 500, y: 240 },
            CubicTo { x1: 224, y1: 240, x2: 0, y2: 186, x: 0, y: 120 },
        ],
    },
    ShapeRecipe {
        id: "circle_plus",
        use_for: "Element-wise addition, a residual join (the Transformer's and ResNet's add node). Size it 16 to 20 px; white fill, ink stroke.",
        commands: &CIRCLE_PLUS,
    },
    ShapeRecipe {
        id: "circle_times",
        use_for: "Element-wise multiplication or gating (GLU, attention weighting, Mamba gating). Size it like circle_plus.",
        commands: &CIRCLE_TIMES,
    },
    ShapeRecipe {
        id: "circle",
        use_for: "A state, a token, a neuron or a sampling point. Use an Ellipse node when no path is needed.",
        commands: &[
            MoveTo { x: 500, y: 0 },
            CubicTo { x1: 500 + K500, y1: 0, x2: 1000, y2: 500 - K500, x: 1000, y: 500 },
            CubicTo { x1: 1000, y1: 500 + K500, x2: 500 + K500, y2: 1000, x: 500, y: 1000 },
            CubicTo { x1: 500 - K500, y1: 1000, x2: 0, y2: 500 + K500, x: 0, y: 500 },
            CubicTo { x1: 0, y1: 500 - K500, x2: 500 - K500, y2: 0, x: 500, y: 0 },
            Close,
        ],
    },
    ShapeRecipe {
        id: "diamond",
        use_for: "A decision, a router or a switch (mixture-of-experts gate, a condition in an algorithm figure).",
        commands: &[MoveTo { x: 500, y: 0 }, LineTo { x: 1000, y: 500 }, LineTo { x: 500, y: 1000 }, LineTo { x: 0, y: 500 }, Close],
    },
    ShapeRecipe {
        id: "hexagon",
        use_for: "An operator or a frozen/pretrained module set apart from trained blocks; a loss or an objective.",
        commands: &[
            MoveTo { x: 250, y: 0 }, LineTo { x: 750, y: 0 }, LineTo { x: 1000, y: 500 }, LineTo { x: 750, y: 1000 }, LineTo { x: 250, y: 1000 },
            LineTo { x: 0, y: 500 }, Close,
        ],
    },
    ShapeRecipe {
        id: "cuboid",
        use_for: "A tensor or feature map with depth (C x H x W): CNN feature maps, image latents, video volumes. Scale its bounds to show relative sizes.",
        commands: &[
            MoveTo { x: 0, y: 250 }, LineTo { x: 250, y: 0 }, LineTo { x: 1000, y: 0 }, LineTo { x: 1000, y: 750 }, LineTo { x: 750, y: 1000 },
            LineTo { x: 0, y: 1000 }, Close,
            MoveTo { x: 0, y: 250 }, LineTo { x: 750, y: 250 }, LineTo { x: 750, y: 1000 },
            MoveTo { x: 750, y: 250 }, LineTo { x: 1000, y: 0 },
        ],
    },
    ShapeRecipe {
        id: "triangle",
        use_for: "Pooling, a funnel or a reduction; rotate the meaning with the reading direction (apex towards the smaller side).",
        commands: &[MoveTo { x: 500, y: 0 }, LineTo { x: 1000, y: 1000 }, LineTo { x: 0, y: 1000 }, Close],
    },
    ShapeRecipe {
        id: "block_arrow_right",
        use_for: "A stage-to-stage transition between panels or a training/inference phase change. Use Line arrows between blocks; use this only for a coarse step.",
        commands: &[
            MoveTo { x: 0, y: 250 }, LineTo { x: 600, y: 250 }, LineTo { x: 600, y: 0 }, LineTo { x: 1000, y: 500 }, LineTo { x: 600, y: 1000 },
            LineTo { x: 600, y: 750 }, LineTo { x: 0, y: 750 }, Close,
        ],
    },
    ShapeRecipe {
        id: "document",
        use_for: "A document, a prompt or a text corpus as an input.",
        commands: &[
            MoveTo { x: 0, y: 0 }, LineTo { x: 1000, y: 0 }, LineTo { x: 1000, y: 860 },
            CubicTo { x1: 750, y1: 760, x2: 500, y2: 1000, x: 250, y: 930 },
            CubicTo { x1: 140, y1: 900, x2: 60, y2: 880, x: 0, y: 900 },
            Close,
        ],
    },
];

/// The recipe named `id`.
pub fn shape(id: &str) -> Option<&'static ShapeRecipe> {
    SHAPES.iter().find(|recipe| recipe.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recipe_starts_with_a_move_stays_in_its_view_box_and_has_a_unique_id() {
        let mut ids: Vec<&str> = SHAPES.iter().map(|recipe| recipe.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), SHAPES.len());
        for recipe in SHAPES {
            assert!(matches!(recipe.commands.first(), Some(MoveTo { .. })), "{}", recipe.id);
            let inside = |v: i64| (0..=SHAPE_VIEW_BOX).contains(&v);
            for command in recipe.commands {
                let ok = match *command {
                    MoveTo { x, y } | LineTo { x, y } => inside(x) && inside(y),
                    CubicTo { x1, y1, x2, y2, x, y } => [x1, y1, x2, y2, x, y].into_iter().all(inside),
                    Close => true,
                };
                assert!(ok, "{} leaves its view box", recipe.id);
            }
            assert!(!recipe.use_for.is_empty());
        }
        assert_eq!(shape("circle_plus").map(|r| r.commands.len()), Some(10));
        assert!(shape("nope").is_none());
    }

    /// Twice the signed area of each subpath of a recipe, from its points (curve control points included): the renderer
    /// fills with the nonzero rule and closes open subpaths, so a subpath wound against the outline would cut a hole.
    fn subpath_areas(commands: &[PathCommand]) -> Vec<i128> {
        let mut areas = Vec::new();
        let mut points: Vec<(i64, i64)> = Vec::new();
        let flush = |points: &mut Vec<(i64, i64)>, areas: &mut Vec<i128>| {
            if points.len() > 2 {
                let area: i128 = (0..points.len()).map(|i| {
                    let (a, b) = (points[i], points[(i + 1) % points.len()]);
                    i128::from(a.0) * i128::from(b.1) - i128::from(b.0) * i128::from(a.1)
                }).sum();
                areas.push(area);
            }
            points.clear();
        };
        for command in commands {
            match *command {
                MoveTo { x, y } => {
                    flush(&mut points, &mut areas);
                    points.push((x, y));
                }
                LineTo { x, y } => points.push((x, y)),
                CubicTo { x1, y1, x2, y2, x, y } => points.extend([(x1, y1), (x2, y2), (x, y)]),
                Close => flush(&mut points, &mut areas),
            }
        }
        flush(&mut points, &mut areas);
        areas
    }

    #[test]
    fn every_subpath_that_encloses_an_area_winds_the_same_way_as_the_outline() {
        for recipe in SHAPES {
            let areas = subpath_areas(recipe.commands);
            let outline = areas.first().copied().unwrap_or_default();
            assert_ne!(outline, 0, "{} has an outline", recipe.id);
            for area in &areas[1..] {
                assert!(*area == 0 || area.signum() == outline.signum(), "{}: a subpath winds against the outline ({areas:?})", recipe.id);
            }
        }
        // The cylinder's rim encloses the band under it, wound with the outline.
        let rim = subpath_areas(shape("cylinder").expect("cylinder").commands);
        assert!(rim.len() == 2 && rim[1] != 0 && rim[1].signum() == rim[0].signum(), "{rim:?}");
    }
}
