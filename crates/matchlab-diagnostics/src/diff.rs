//! Line-based unified diff (Myers), formatted like Python's
//! `difflib.unified_diff` so `git apply` accepts it.

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tag {
    Equal,
    Replace,
    Delete,
    Insert,
}

type Opcode = (Tag, usize, usize, usize, usize);

/// Matching (old, new) line index pairs of a shortest edit script.
fn matches(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m) as usize;
    let offset = max as isize + 1;
    let mut v = vec![0isize; 2 * max + 3];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    'outer: for d in 0..=max as isize {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let index = (k + offset) as usize;
            let mut x = if k == -d || (k != d && v[index - 1] < v[index + 1]) { v[index + 1] } else { v[index - 1] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[index] = x;
            if x >= n && y >= m {
                break 'outer;
            }
            k += 2;
        }
    }
    // Backtrack through the saved frontiers.
    let mut pairs = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (0..trace.len() as isize).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let previous_k = if k == -d || (k != d && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize]) { k + 1 } else { k - 1 };
        let previous_x = if d == 0 { 0 } else { v[(previous_k + offset) as usize] };
        let previous_y = previous_x - previous_k;
        while x > previous_x.max(0) && y > previous_y.max(0) && x > 0 && y > 0 && a[(x - 1) as usize] == b[(y - 1) as usize] {
            x -= 1;
            y -= 1;
            pairs.push((x as usize, y as usize));
        }
        if d == 0 {
            break;
        }
        x = previous_x;
        y = previous_y;
    }
    pairs.reverse();
    pairs
}

fn opcodes(a: &[&str], b: &[&str]) -> Vec<Opcode> {
    let mut codes = Vec::new();
    let (mut i, mut j) = (0, 0);
    let mut pairs = matches(a, b);
    pairs.push((a.len(), b.len()));
    let mut index = 0;
    while index < pairs.len() {
        let (x, y) = pairs[index];
        if i < x || j < y {
            let tag = match (i < x, j < y) {
                (true, true) => Tag::Replace,
                (true, false) => Tag::Delete,
                _ => Tag::Insert,
            };
            codes.push((tag, i, x, j, y));
        }
        if x == a.len() && y == b.len() {
            break;
        }
        // A maximal run of consecutive matches.
        let mut end = index;
        while end + 1 < pairs.len() - 1 && pairs[end + 1] == (pairs[end].0 + 1, pairs[end].1 + 1) {
            end += 1;
        }
        let (ex, ey) = (pairs[end].0 + 1, pairs[end].1 + 1);
        codes.push((Tag::Equal, x, ex, y, ey));
        i = ex;
        j = ey;
        index = end + 1;
    }
    codes
}

fn grouped(mut codes: Vec<Opcode>, n: usize) -> Vec<Vec<Opcode>> {
    if codes.is_empty() {
        codes.push((Tag::Equal, 0, 1, 0, 1));
    }
    if let Some(first) = codes.first_mut().filter(|code| code.0 == Tag::Equal) {
        *first = (Tag::Equal, first.1.max(first.2.saturating_sub(n)), first.2, first.3.max(first.4.saturating_sub(n)), first.4);
    }
    if let Some(last) = codes.last_mut().filter(|code| code.0 == Tag::Equal) {
        *last = (Tag::Equal, last.1, last.2.min(last.1 + n), last.3, last.4.min(last.3 + n));
    }
    let mut groups = Vec::new();
    let mut group = Vec::new();
    for (tag, mut i1, i2, mut j1, j2) in codes {
        if tag == Tag::Equal && i2 - i1 > 2 * n {
            group.push((tag, i1, i2.min(i1 + n), j1, j2.min(j1 + n)));
            groups.push(std::mem::take(&mut group));
            i1 = i1.max(i2.saturating_sub(n));
            j1 = j1.max(j2.saturating_sub(n));
        }
        group.push((tag, i1, i2, j1, j2));
    }
    if !group.is_empty() && !(group.len() == 1 && group[0].0 == Tag::Equal) {
        groups.push(group);
    }
    groups
}

fn range(start: usize, stop: usize) -> String {
    let length = stop - start;
    let beginning = start + 1;
    match length {
        1 => beginning.to_string(),
        0 => format!("{},0", beginning - 1),
        _ => format!("{beginning},{length}"),
    }
}

pub fn unified(old: &str, new: &str, from: &str, to: &str, context: usize) -> String {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let mut out = String::new();
    for (index, group) in grouped(opcodes(&a, &b), context).iter().enumerate() {
        if index == 0 {
            out.push_str(&format!("--- {from}\n+++ {to}\n"));
        }
        let (first, last) = (group[0], group[group.len() - 1]);
        out.push_str(&format!("@@ -{} +{} @@\n", range(first.1, last.2), range(first.3, last.4)));
        for &(tag, i1, i2, j1, j2) in group {
            if tag == Tag::Equal {
                a[i1..i2].iter().for_each(|line| out.push_str(&format!(" {line}")));
                continue;
            }
            if matches!(tag, Tag::Replace | Tag::Delete) {
                a[i1..i2].iter().for_each(|line| out.push_str(&format!("-{line}")));
            }
            if matches!(tag, Tag::Replace | Tag::Insert) {
                b[j1..j2].iter().for_each(|line| out.push_str(&format!("+{line}")));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::unified;

    #[test]
    fn difflib_layout() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let new = "a\nb\nX\nc\nd\ne\nf\ng\nh\nY\nj\nk\n";
        assert_eq!(
            unified(old, new, "a/f", "b/f", 3),
            "--- a/f\n+++ b/f\n@@ -1,10 +1,12 @@\n a\n b\n+X\n c\n d\n e\n f\n g\n h\n-i\n+Y\n j\n+k\n"
        );
        assert_eq!(unified(old, old, "a", "b", 3), "");
        let far = (0..30).map(|i| format!("{i}\n")).collect::<String>();
        let edited = far.replacen("2\n", "two\n", 1).replace("27\n", "27b\n");
        let diff = unified(&far, &edited, "a", "b", 3);
        assert_eq!(diff.matches("@@ -").count(), 2, "{diff}");
        assert!(diff.contains("@@ -1,6 +1,6 @@\n 0\n 1\n-2\n+two\n 3\n 4\n 5\n@@ -25,6 +25,6 @@\n 24\n"), "{diff}");
    }

    /// Apply a unified diff, checking every context and removed line.
    fn apply(old: &str, patch: &str) -> String {
        let source: Vec<&str> = old.split_inclusive('\n').collect();
        let mut out = String::new();
        let mut cursor = 0;
        for line in patch.split_inclusive('\n').skip(2) {
            if let Some(header) = line.strip_prefix("@@ -") {
                let range = header.split(' ').next().unwrap();
                let (start, length) = range.split_once(',').map_or((range, "1"), |(a, b)| (a, b));
                let start: usize = start.parse().unwrap();
                let begin = if length == "0" { start } else { start - 1 };
                source[cursor..begin].iter().for_each(|line| out.push_str(line));
                cursor = begin;
            } else if let Some(text) = line.strip_prefix('+') {
                out.push_str(text);
            } else {
                assert_eq!(source[cursor], &line[1..], "context or removal mismatch");
                if line.starts_with(' ') {
                    out.push_str(&line[1..]);
                }
                cursor += 1;
            }
        }
        source[cursor..].iter().for_each(|line| out.push_str(line));
        out
    }

    #[test]
    fn random_edits_round_trip() {
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = |limit: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % limit
        };
        for _ in 0..400 {
            let old: Vec<String> = (0..next(60)).map(|_| format!("{}\n", next(6))).collect();
            let mut new = old.clone();
            for _ in 0..next(8) {
                let at = next(new.len() as u64 + 1) as usize;
                match next(3) {
                    0 if at < new.len() => {
                        new.remove(at);
                    }
                    1 if at < new.len() => new[at] = format!("x{}\n", next(4)),
                    _ => new.insert(at, format!("{}\n", next(9))),
                }
            }
            let (old, new) = (old.concat(), new.concat());
            let patch = unified(&old, &new, "a", "b", 3);
            assert_eq!(apply(&old, &patch), new, "{patch}");
            assert_eq!(patch.is_empty(), old == new);
        }
    }
}
