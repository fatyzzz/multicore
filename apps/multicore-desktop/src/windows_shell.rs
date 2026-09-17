use crate::preferences::WindowBounds;

pub const MIN_WIDTH: u32 = 700;
pub const MIN_HEIGHT: u32 = 620;

const REACHABLE_TITLE_BAR: i64 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowDisposition {
    HideToTray,
    MinimizeToTaskbar,
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeEdge {
    North,
    NorthEast,
    East,
    SouthEast,
    South,
    SouthWest,
    West,
    NorthWest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonitorRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

pub fn minimize_disposition(tray_available: bool) -> WindowDisposition {
    if tray_available {
        WindowDisposition::HideToTray
    } else {
        WindowDisposition::MinimizeToTaskbar
    }
}

pub fn close_disposition(tray_available: bool, smoke_mode: bool) -> WindowDisposition {
    if tray_available && !smoke_mode {
        WindowDisposition::HideToTray
    } else {
        WindowDisposition::Exit
    }
}

pub fn restore_bounds(saved: WindowBounds, monitors: &[MonitorRect]) -> WindowBounds {
    let Some((monitor, intersects)) = select_monitor(&saved, monitors) else {
        return WindowBounds {
            x: saved.x,
            y: saved.y,
            width: saved.width.max(MIN_WIDTH),
            height: saved.height.max(MIN_HEIGHT),
        };
    };

    let width = bounded_dimension(saved.width, monitor.width, MIN_WIDTH);
    let height = bounded_dimension(saved.height, monitor.height, MIN_HEIGHT);

    if !intersects {
        return WindowBounds {
            x: saturating_i32(
                i64::from(monitor.x) + (i64::from(monitor.width) - i64::from(width)) / 2,
            ),
            y: saturating_i32(
                i64::from(monitor.y) + (i64::from(monitor.height) - i64::from(height)) / 2,
            ),
            width,
            height,
        };
    }

    let monitor_left = i64::from(monitor.x);
    let monitor_top = i64::from(monitor.y);
    let monitor_right = monitor_left + i64::from(monitor.width);
    let monitor_bottom = monitor_top + i64::from(monitor.height);
    let x_min = monitor_left - i64::from(width) + REACHABLE_TITLE_BAR.min(i64::from(width));
    let x_max = monitor_right - REACHABLE_TITLE_BAR.min(i64::from(width));
    let y_max = monitor_bottom - REACHABLE_TITLE_BAR.min(i64::from(monitor.height));

    WindowBounds {
        x: saturating_i32(clamp_i64(i64::from(saved.x), x_min, x_max)),
        y: saturating_i32(clamp_i64(i64::from(saved.y), monitor_top, y_max)),
        width,
        height,
    }
}

fn select_monitor<'a>(
    saved: &WindowBounds,
    monitors: &'a [MonitorRect],
) -> Option<(&'a MonitorRect, bool)> {
    let intersecting = monitors
        .iter()
        .enumerate()
        .map(|(index, monitor)| (intersection_area(saved, monitor), index, monitor))
        .filter(|(area, _, _)| *area > 0)
        .max_by_key(|(area, index, _)| (*area, std::cmp::Reverse(*index)));

    if let Some((_, _, monitor)) = intersecting {
        return Some((monitor, true));
    }

    monitors
        .iter()
        .find(|monitor| monitor.primary)
        .or_else(|| monitors.first())
        .map(|monitor| (monitor, false))
}

fn intersection_area(saved: &WindowBounds, monitor: &MonitorRect) -> u64 {
    let saved_left = i64::from(saved.x);
    let saved_top = i64::from(saved.y);
    let saved_right = saved_left + i64::from(saved.width);
    let saved_bottom = saved_top + i64::from(saved.height);
    let monitor_left = i64::from(monitor.x);
    let monitor_top = i64::from(monitor.y);
    let monitor_right = monitor_left + i64::from(monitor.width);
    let monitor_bottom = monitor_top + i64::from(monitor.height);

    let width = (saved_right.min(monitor_right) - saved_left.max(monitor_left)).max(0) as u64;
    let height = (saved_bottom.min(monitor_bottom) - saved_top.max(monitor_top)).max(0) as u64;
    width.saturating_mul(height)
}

fn bounded_dimension(saved: u32, available: u32, minimum: u32) -> u32 {
    if available >= minimum {
        saved.clamp(minimum, available)
    } else {
        available
    }
}

fn clamp_i64(value: i64, minimum: i64, maximum: i64) -> i64 {
    if minimum <= maximum {
        value.clamp(minimum, maximum)
    } else {
        minimum
    }
}

fn saturating_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

pub fn parse_resize_edge(edge: &str) -> Option<ResizeEdge> {
    match edge {
        "n" => Some(ResizeEdge::North),
        "ne" => Some(ResizeEdge::NorthEast),
        "e" => Some(ResizeEdge::East),
        "se" => Some(ResizeEdge::SouthEast),
        "s" => Some(ResizeEdge::South),
        "sw" => Some(ResizeEdge::SouthWest),
        "w" => Some(ResizeEdge::West),
        "nw" => Some(ResizeEdge::NorthWest),
        _ => None,
    }
}

pub fn next_maximized(current: bool) -> bool {
    !current
}

#[cfg(windows)]
impl From<ResizeEdge> for slint::winit_030::winit::window::ResizeDirection {
    fn from(edge: ResizeEdge) -> Self {
        use slint::winit_030::winit::window::ResizeDirection;

        match edge {
            ResizeEdge::North => ResizeDirection::North,
            ResizeEdge::NorthEast => ResizeDirection::NorthEast,
            ResizeEdge::East => ResizeDirection::East,
            ResizeEdge::SouthEast => ResizeDirection::SouthEast,
            ResizeEdge::South => ResizeDirection::South,
            ResizeEdge::SouthWest => ResizeDirection::SouthWest,
            ResizeEdge::West => ResizeDirection::West,
            ResizeEdge::NorthWest => ResizeDirection::NorthWest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MIN_HEIGHT, MIN_WIDTH, MonitorRect, ResizeEdge, WindowDisposition, close_disposition,
        minimize_disposition, next_maximized, parse_resize_edge, restore_bounds,
    };
    use crate::preferences::WindowBounds;

    fn monitor(x: i32, y: i32, width: u32, height: u32, primary: bool) -> MonitorRect {
        MonitorRect {
            x,
            y,
            width,
            height,
            primary,
        }
    }

    #[test]
    fn removed_monitor_bounds_are_centered_on_primary() {
        let restored = restore_bounds(
            WindowBounds {
                x: 2_000,
                y: 200,
                width: 900,
                height: 700,
            },
            &[
                monitor(-1_280, 0, 1_280, 1_024, false),
                monitor(0, 0, 1_920, 1_080, true),
            ],
        );

        assert_eq!(
            restored,
            WindowBounds {
                x: 510,
                y: 190,
                width: 900,
                height: 700,
            }
        );
    }

    #[test]
    fn negative_monitor_coordinates_are_preserved() {
        let saved = WindowBounds {
            x: -1_100,
            y: -800,
            width: 800,
            height: 650,
        };
        assert_eq!(
            restore_bounds(
                saved.clone(),
                &[monitor(-1_280, -1_024, 1_280, 1_024, true)]
            ),
            saved
        );
    }

    #[test]
    fn partial_intersection_keeps_title_bar_reachable_on_that_monitor() {
        let restored = restore_bounds(
            WindowBounds {
                x: 950,
                y: 790,
                width: 800,
                height: 700,
            },
            &[monitor(0, 0, 1_000, 800, true)],
        );

        assert_eq!(restored.x, 936);
        assert_eq!(restored.y, 736);
        assert_eq!(restored.width, 800);
        assert_eq!(restored.height, 700);
    }

    #[test]
    fn oversized_and_zero_geometry_is_bounded_to_monitor() {
        assert_eq!(
            restore_bounds(
                WindowBounds {
                    x: 10,
                    y: 10,
                    width: u32::MAX,
                    height: u32::MAX,
                },
                &[monitor(0, 0, 1_200, 900, true)],
            ),
            WindowBounds {
                x: 10,
                y: 10,
                width: 1_200,
                height: 900,
            }
        );
        assert_eq!(
            restore_bounds(
                WindowBounds {
                    x: 100,
                    y: 100,
                    width: 0,
                    height: 0,
                },
                &[monitor(0, 0, 1_200, 900, true)],
            ),
            WindowBounds {
                x: 250,
                y: 140,
                width: MIN_WIDTH,
                height: MIN_HEIGHT,
            }
        );
    }

    #[test]
    fn monitors_smaller_than_minimum_limit_bounds_to_available_area() {
        assert_eq!(
            restore_bounds(
                WindowBounds {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
                &[monitor(-400, -300, 400, 300, true)],
            ),
            WindowBounds {
                x: -400,
                y: -300,
                width: 400,
                height: 300,
            }
        );
    }

    #[test]
    fn empty_monitor_list_is_safe_and_enforces_minimum_size() {
        assert_eq!(
            restore_bounds(
                WindowBounds {
                    x: i32::MAX,
                    y: i32::MIN,
                    width: 0,
                    height: 10,
                },
                &[],
            ),
            WindowBounds {
                x: i32::MAX,
                y: i32::MIN,
                width: MIN_WIDTH,
                height: MIN_HEIGHT,
            }
        );
    }

    #[test]
    fn parses_all_resize_edges_exactly() {
        let cases = [
            ("n", ResizeEdge::North),
            ("ne", ResizeEdge::NorthEast),
            ("e", ResizeEdge::East),
            ("se", ResizeEdge::SouthEast),
            ("s", ResizeEdge::South),
            ("sw", ResizeEdge::SouthWest),
            ("w", ResizeEdge::West),
            ("nw", ResizeEdge::NorthWest),
        ];
        for (text, edge) in cases {
            assert_eq!(parse_resize_edge(text), Some(edge));
        }
        for invalid in ["", "N", " north", "ne ", "north", "x"] {
            assert_eq!(parse_resize_edge(invalid), None);
        }
    }

    #[test]
    fn lifecycle_dispositions_cover_tray_and_smoke_modes() {
        assert_eq!(minimize_disposition(true), WindowDisposition::HideToTray);
        assert_eq!(
            minimize_disposition(false),
            WindowDisposition::MinimizeToTaskbar
        );
        assert_eq!(
            close_disposition(true, false),
            WindowDisposition::HideToTray
        );
        assert_eq!(close_disposition(false, false), WindowDisposition::Exit);
        assert_eq!(close_disposition(true, true), WindowDisposition::Exit);
        assert_eq!(close_disposition(false, true), WindowDisposition::Exit);
    }

    #[test]
    fn maximize_toggle_inverts_current_state() {
        assert!(next_maximized(false));
        assert!(!next_maximized(true));
    }
}
