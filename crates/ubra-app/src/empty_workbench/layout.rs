use ubra_proto::workspace::LayoutAxis;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum LayoutPreset {
    #[default]
    Single,
    SideBySide,
    Stacked,
    FocusTwo,
    Grid,
    Six,
    Eight,
    Sixteen,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum LayoutTopology {
    Leaf,
    Split {
        axis: LayoutAxis,
        fraction: f32,
        first: Box<Self>,
        second: Box<Self>,
    },
}

impl LayoutPreset {
    pub(crate) fn all() -> &'static [Self] {
        &[
            Self::Single,
            Self::SideBySide,
            Self::Stacked,
            Self::FocusTwo,
            Self::Grid,
            Self::Six,
            Self::Eight,
            Self::Sixteen,
        ]
    }

    pub(crate) fn count(self) -> usize {
        match self {
            Self::Single => 1,
            Self::SideBySide | Self::Stacked => 2,
            Self::FocusTwo => 3,
            Self::Grid => 4,
            Self::Six => 6,
            Self::Eight => 8,
            Self::Sixteen => 16,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Single => "Single · 1×1",
            Self::SideBySide => "Side by side · 2×1",
            Self::Stacked => "Stacked · 1×2",
            Self::FocusTwo => "Focus + two",
            Self::Grid => "Grid · 2×2",
            Self::Six => "Six · 3×2",
            Self::Eight => "Eight · 4×2",
            Self::Sixteen => "Sixteen · 4×4",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Single => "One focused session.",
            Self::SideBySide => "Compare two sessions side by side.",
            Self::Stacked => "Two sessions, one above the other.",
            Self::FocusTwo => "A larger focus pane with two supporting sessions.",
            Self::Grid => "Four independent sessions in an equal grid.",
            Self::Six => "Three columns, two rows of independent sessions.",
            Self::Eight => "Eight independent processes may use significant agent resources.",
            Self::Sixteen => "Sixteen independent processes may use significant agent resources.",
        }
    }

    pub(crate) fn topology(self) -> LayoutTopology {
        match self {
            Self::Single => LayoutTopology::Leaf,
            Self::SideBySide => grid(2, 1),
            Self::Stacked => grid(1, 2),
            Self::FocusTwo => split(
                LayoutAxis::Horizontal,
                0.65,
                LayoutTopology::Leaf,
                grid(1, 2),
            ),
            Self::Grid => grid(2, 2),
            Self::Six => grid(3, 2),
            Self::Eight => grid(4, 2),
            Self::Sixteen => grid(4, 4),
        }
    }
}

fn split(
    axis: LayoutAxis,
    fraction: f32,
    first: LayoutTopology,
    second: LayoutTopology,
) -> LayoutTopology {
    LayoutTopology::Split {
        axis,
        fraction,
        first: Box::new(first),
        second: Box::new(second),
    }
}

fn grid(columns: usize, rows: usize) -> LayoutTopology {
    if columns > 1 {
        let first = columns / 2;
        split(
            LayoutAxis::Horizontal,
            first as f32 / columns as f32,
            grid(first, rows),
            grid(columns - first, rows),
        )
    } else if rows > 1 {
        let first = rows / 2;
        split(
            LayoutAxis::Vertical,
            first as f32 / rows as f32,
            grid(1, first),
            grid(1, rows - first),
        )
    } else {
        LayoutTopology::Leaf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(node: &LayoutTopology, width: f32, height: f32, sizes: &mut Vec<(f32, f32)>) {
        match node {
            LayoutTopology::Leaf => sizes.push((width, height)),
            LayoutTopology::Split {
                axis,
                fraction,
                first,
                second,
            } => {
                let (w, h) = match axis {
                    LayoutAxis::Horizontal => (width * fraction, height),
                    LayoutAxis::Vertical => (width, height * fraction),
                };
                leaves(first, w, h, sizes);
                let (w, h) = match axis {
                    LayoutAxis::Horizontal => (width * (1.0 - fraction), height),
                    LayoutAxis::Vertical => (width, height * (1.0 - fraction)),
                };
                leaves(second, w, h, sizes);
            }
        }
    }

    #[test]
    fn every_preset_has_exact_count_and_equal_grid_areas() {
        for preset in LayoutPreset::all() {
            let mut sizes = Vec::new();
            leaves(&preset.topology(), 1.0, 1.0, &mut sizes);
            assert_eq!(sizes.len(), preset.count());
            if *preset != LayoutPreset::FocusTwo {
                for (width, height) in sizes {
                    assert!((width * height - 1.0 / preset.count() as f32).abs() < 0.00001);
                }
            }
        }
        let mut sizes = Vec::new();
        leaves(&LayoutPreset::Six.topology(), 1.0, 1.0, &mut sizes);
        assert!(
            sizes
                .iter()
                .all(|(w, h)| (*w - 1.0 / 3.0).abs() < 0.00001 && *h == 0.5)
        );
    }
}
