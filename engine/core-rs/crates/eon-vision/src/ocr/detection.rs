use super::{EonResult, MAX_CANDIDATES, PixelBounds, limited};

/// DB predicts a shrunk text mask. For upright UI labels, group connected
/// ink, expand by area/perimeter and project the rectangle back to the raster.
/// We don't claim rotated quadrilaterals or individual-word alignment.
pub(super) fn regions(
    scores: &[f32],
    dw: u32,
    dh: u32,
    width: u32,
    height: u32,
) -> EonResult<Vec<PixelBounds>> {
    let mut mask: Vec<bool> = scores.iter().map(|&v| v >= 0.3).collect();
    let mut stack = Vec::new();
    let mut result = Vec::new();
    for seed in 0..mask.len() {
        if !mask[seed] {
            continue;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (dw, dh, 0, 0);
        let (mut area, mut sum) = (0u32, 0.0);
        mask[seed] = false;
        stack.push(seed);
        while let Some(i) = stack.pop() {
            let (x, y) = (i as u32 % dw, i as u32 / dw);
            (x0, y0) = (x0.min(x), y0.min(y));
            (x1, y1) = (x1.max(x + 1), y1.max(y + 1));
            area += 1;
            sum += scores[i];
            let mut visit = |n: usize| {
                if mask[n] {
                    mask[n] = false;
                    stack.push(n);
                }
            };
            if x > 0 {
                visit(i - 1);
            }
            if x + 1 < dw {
                visit(i + 1);
            }
            if y > 0 {
                visit(i - dw as usize);
            }
            if y + 1 < dh {
                visit(i + dw as usize);
            }
        }
        if x1 - x0 < 3 || y1 - y0 < 3 || sum < 0.5 * area as f32 {
            continue;
        }
        if result.len() >= MAX_CANDIDATES {
            return Err(limited(
                "OCR text regions",
                (result.len() + 1) as u64,
                MAX_CANDIDATES as u64,
            ));
        }
        let perimeter = 2 * ((x1 - x0) + (y1 - y0));
        let pad = (1.6 * area as f32 / perimeter as f32).round() as u32;
        result.push(PixelBounds {
            left: x0.saturating_sub(pad) * width / dw,
            top: y0.saturating_sub(pad) * height / dh,
            right: ((x1 + pad).min(dw) * width).div_ceil(dw),
            bottom: ((y1 + pad).min(dh) * height).div_ceil(dh),
        });
    }

    // A few pixels' difference between the tops of neighboring labels should
    // not reverse their left-to-right order. Anchor each row to its first box.
    result.sort_by_key(|r| (r.top, r.left));
    let mut rest = result.as_mut_slice();
    while let Some(first) = rest.first() {
        let mid = first.top + (first.bottom - first.top) / 2;
        let end = rest.partition_point(|r| r.top <= mid);
        let (row, next) = rest.split_at_mut(end);
        row.sort_by_key(|r| (r.left, r.top));
        rest = next;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dense_candidate_page_refused_before_recognition() {
        // Separate 4x4 components on a 9x9 grid (81 candidates).
        let scores: Vec<_> = (0..54 * 54)
            .map(|p| {
                if (p % 54) % 6 < 4 && (p / 54) % 6 < 4 {
                    1.
                } else {
                    0.
                }
            })
            .collect();
        assert!(matches!(
            regions(&scores, 54, 54, 54, 54),
            Err(crate::EonError::ResourceExhausted { .. })
        ));
    }
}
