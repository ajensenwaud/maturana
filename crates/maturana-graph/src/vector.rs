//! From-scratch vector similarity. v1 is exact brute-force cosine, which is
//! ample for personal-scale knowledge graphs (thousands of nodes). The store
//! calls this behind a narrow interface so a hand-written ANN index (e.g. HNSW)
//! can replace it later without touching callers.

/// Cosine similarity in `[-1, 1]`. Returns `0.0` for empty, length-mismatched,
/// or zero-magnitude inputs (treated as "no signal" rather than an error).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Rank `(id, embedding)` candidates by cosine to `query`, returning the top
/// `k` as `(id, score)` sorted descending. Skips scores at or below
/// `min_score`.
pub fn top_k<'a, I>(query: &[f32], candidates: I, k: usize, min_score: f32) -> Vec<(String, f32)>
where
    I: IntoIterator<Item = (&'a str, &'a [f32])>,
{
    let mut scored: Vec<(String, f32)> = candidates
        .into_iter()
        .map(|(id, emb)| (id.to_string(), cosine(query, emb)))
        .filter(|(_, score)| *score > min_score)
        .collect();
    // NaN-safe descending sort.
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basic() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
        // Degenerate inputs are 0, not NaN/panic.
        assert_eq!(cosine(&[], &[]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn top_k_ranks_and_truncates() {
        let cands = vec![
            ("a", vec![1.0f32, 0.0]),
            ("b", vec![0.9f32, 0.1]),
            ("c", vec![0.0f32, 1.0]),
        ];
        let refs: Vec<(&str, &[f32])> = cands.iter().map(|(id, v)| (*id, v.as_slice())).collect();
        let out = top_k(&[1.0, 0.0], refs, 2, 0.0);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "a");
        assert_eq!(out[1].0, "b");
    }
}
