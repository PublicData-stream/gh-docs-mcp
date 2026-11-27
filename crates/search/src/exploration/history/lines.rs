use std::ops::Range;

use docs_mcp_git::reader::Batch;

use super::super::Error;

const LINE_LIMIT: usize = 65_536;
const WORK_LIMIT: usize = 4_000_000;
const TRACE_LIMIT: usize = WORK_LIMIT;

pub(super) fn ranges(bytes: &[u8]) -> Result<Vec<Range<usize>>, Error> {
    let mut ranges = Vec::new();
    let mut first = 0;
    for line in bytes.split_inclusive(|&byte| byte == b'\n') {
        if ranges.len() == LINE_LIMIT {
            return Err(Error::Budget);
        }
        let end = first + line.len();
        ranges.push(first..end);
        first = end;
    }
    Ok(ranges)
}

pub(super) struct Work {
    remaining: usize,
}

impl Default for Work {
    fn default() -> Self {
        Self {
            remaining: WORK_LIMIT,
        }
    }
}

impl Work {
    fn spend(&mut self, amount: usize, batch: &Batch<'_>) -> Result<(), Error> {
        let left = self.remaining.checked_sub(amount).ok_or(Error::Budget)?;
        if left / 256 != self.remaining / 256 {
            batch.check()?;
        }
        self.remaining = left;
        Ok(())
    }
}

/// Return the matching line pairs of a shortest edit script. On a tie the
/// forward pass extends the deletion diagonal. Both time and trace are charged.
pub(super) fn matching(
    old: &[u8],
    old_lines: &[Range<usize>],
    new: &[u8],
    new_lines: &[Range<usize>],
    work: &mut Work,
    batch: &Batch<'_>,
) -> Result<Vec<(usize, usize)>, Error> {
    if old_lines.is_empty() || new_lines.is_empty() {
        return Ok(Vec::new());
    }
    myers(
        old_lines.len(),
        new_lines.len(),
        |x, y| {
            let left = &old[old_lines[x].clone()];
            let right = &new[new_lines[y].clone()];
            work.spend(1 + left.len().min(right.len()) / 256, batch)?;
            Ok(left == right)
        },
        || batch.check().map_err(Error::from),
    )
}

fn myers(
    old_len: usize,
    new_len: usize,
    mut equal: impl FnMut(usize, usize) -> Result<bool, Error>,
    mut checkpoint: impl FnMut() -> Result<(), Error>,
) -> Result<Vec<(usize, usize)>, Error> {
    let mut trace: Vec<Vec<i32>> = Vec::new();
    let limit = old_len + new_len;
    for distance in 0..=limit {
        checkpoint()?;
        if (distance + 1) * (distance + 2) / 2 > TRACE_LIMIT {
            return Err(Error::Budget);
        }
        let d = distance as i32;
        let mut frontier = Vec::with_capacity(distance + 1);
        for (index, k) in (-d..=d).step_by(2).enumerate() {
            let mut x = if d == 0 {
                0
            } else {
                let previous = &trace[distance - 1];
                if insertion(k, d, index, previous) {
                    previous[index]
                } else {
                    previous[index - 1] + 1
                }
            };
            let mut y = x - k;
            while x < old_len as i32 && y < new_len as i32 && equal(x as usize, y as usize)? {
                x += 1;
                y += 1;
            }
            if x == old_len as i32 && y == new_len as i32 {
                return Ok(backtrack(x, y, &trace));
            }
            frontier.push(x);
        }
        trace.push(frontier);
    }
    Err(Error::Internal)
}

fn insertion(k: i32, d: i32, index: usize, previous: &[i32]) -> bool {
    k == -d || (k != d && previous[index - 1] < previous[index])
}

fn backtrack(mut x: i32, mut y: i32, trace: &[Vec<i32>]) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    for (distance, previous) in trace.iter().enumerate().rev() {
        let d = (distance + 1) as i32;
        let k = x - y;
        let index = ((k + d) / 2) as usize;
        let (previous_x, previous_y) = if insertion(k, d, index, previous) {
            (previous[index], previous[index] - (k + 1))
        } else {
            (previous[index - 1], previous[index - 1] - (k - 1))
        };
        while x > previous_x && y > previous_y {
            x -= 1;
            y -= 1;
            pairs.push((x as usize, y as usize));
        }
        (x, y) = (previous_x, previous_y);
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        pairs.push((x as usize, y as usize));
    }
    pairs.reverse();
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_matching_retains_original_lf_and_cr() {
        let find = |a: &[u8], b: &[u8]| {
            myers(a.len(), b.len(), |x, y| Ok(a[x] == b[y]), || Ok(())).unwrap()
        };
        assert_eq!(find(b"abc", b"abc"), vec![(0, 0), (1, 1), (2, 2)]);
        assert_eq!(find(b"abc", b"bcxyz"), vec![(1, 0), (2, 1)]);
        assert_eq!(find(b"abc", b"xaxc"), vec![(0, 1), (2, 3)]);
        assert!(find(b"x", b"yy").is_empty());
        assert_eq!(ranges(b"a\r\nb\nlast").unwrap(), vec![0..3, 3..5, 5..9]);
    }
}
