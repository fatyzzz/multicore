const BLUE: [u8; 3] = [76, 157, 255];
const NEUTRAL: [u8; 3] = [236, 241, 247];

pub(crate) fn rgba(size: u32) -> Vec<u8> {
    let mut pixels = vec![0; size.saturating_mul(size).saturating_mul(4) as usize];
    if size == 0 {
        return pixels;
    }

    let scale = size as f32;
    for y in 0..size {
        for x in 0..size {
            let px = (x as f32 + 0.5) / scale;
            let py = (y as f32 + 0.5) / scale;
            let index = ((y * size + x) * 4) as usize;

            let connector = segment_coverage(px, py, (0.42, 0.46), (0.58, 0.54), 0.075, scale);
            if connector > 0.0 {
                let color = if px < 0.5 { BLUE } else { NEUTRAL };
                blend(&mut pixels[index..index + 4], color, connector);
            }

            let left = ring_coverage(px, py, (0.34, 0.43), 0.225, 0.075, scale);
            if left > 0.0 {
                blend(&mut pixels[index..index + 4], BLUE, left);
            }
            let right = ring_coverage(px, py, (0.66, 0.57), 0.225, 0.075, scale);
            if right > 0.0 {
                blend(&mut pixels[index..index + 4], NEUTRAL, right);
            }
        }
    }
    pixels
}

fn ring_coverage(
    px: f32,
    py: f32,
    center: (f32, f32),
    radius: f32,
    stroke: f32,
    scale: f32,
) -> f32 {
    let distance = ((px - center.0).powi(2) + (py - center.1).powi(2)).sqrt();
    edge_coverage((distance - radius).abs(), stroke / 2.0, scale)
}

fn segment_coverage(
    px: f32,
    py: f32,
    start: (f32, f32),
    end: (f32, f32),
    width: f32,
    scale: f32,
) -> f32 {
    let dx = end.0 - start.0;
    let dy = end.1 - start.1;
    let length_squared = dx * dx + dy * dy;
    let t = (((px - start.0) * dx + (py - start.1) * dy) / length_squared).clamp(0.0, 1.0);
    let nearest_x = start.0 + t * dx;
    let nearest_y = start.1 + t * dy;
    let distance = ((px - nearest_x).powi(2) + (py - nearest_y).powi(2)).sqrt();
    edge_coverage(distance, width / 2.0, scale)
}

fn edge_coverage(distance: f32, edge: f32, scale: f32) -> f32 {
    ((edge + 1.0 / scale - distance) * scale).clamp(0.0, 1.0)
}

fn blend(target: &mut [u8], color: [u8; 3], coverage: f32) {
    let source_alpha = coverage.clamp(0.0, 1.0);
    let target_alpha = target[3] as f32 / 255.0;
    let output_alpha = source_alpha + target_alpha * (1.0 - source_alpha);
    if output_alpha <= f32::EPSILON {
        return;
    }
    for channel in 0..3 {
        let source = color[channel] as f32 / 255.0;
        let current = target[channel] as f32 / 255.0;
        let output =
            (source * source_alpha + current * target_alpha * (1.0 - source_alpha)) / output_alpha;
        target[channel] = (output * 255.0).round() as u8;
    }
    target[3] = (output_alpha * 255.0).round() as u8;
}

#[cfg(test)]
mod tests {
    use super::rgba;

    #[test]
    fn icon_is_rgba_with_transparency_and_both_brand_colors() {
        let pixels = rgba(32);
        assert_eq!(pixels.len(), 32 * 32 * 4);

        let mut has_transparency = false;
        let mut has_blue = false;
        let mut has_neutral = false;
        for pixel in pixels.as_chunks::<4>().0 {
            has_transparency |= pixel[3] == 0;
            has_blue |= pixel[2] > 180 && pixel[0] < 120 && pixel[3] > 200;
            has_neutral |= pixel[0] > 190 && pixel[1] > 190 && pixel[2] > 190 && pixel[3] > 200;
        }

        assert!(has_transparency);
        assert!(has_blue);
        assert!(has_neutral);
    }
}
