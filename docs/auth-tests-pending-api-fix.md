# auth.rs tests — pending the api-crate fix

These 4 tests cover the security-critical pure functions in
`crates/api/src/auth.rs` (JWT issue/verify + bcrypt). They have been
**verified to pass locally**, but they are **not wired into CI** because the
`api` crate does not compile on `master`.

## Why they are not committed into `auth.rs`

`crates/api` has been **broken since 2026-03-11** (24 compile errors). Adding
tests to a crate that cannot build gives no CI signal — `cargo test -p api`
fails at the build step before any test runs. So the CI `test` job only runs
`cargo test -p shared --locked` (the `shared` crate builds cleanly).

There is an **uncommitted, ~2-month-old WIP** in the working tree that is
mid-way through fixing the `api` crate. **Committing or discarding that WIP is
the owner's decision** — see the handoff note at the bottom.

## What to do once `api` builds again

Paste the block below at the end of `crates/api/src/auth.rs`, run
`cargo test -p api`, then add `cargo test -p api --locked` to the `test` job in
`.github/workflows/deploy.yml` (or re-enable the workspace test in `ci.yml`).

```rust
// ---------------------------------------------------------------------------
// Tests — security-critical pure functions (no network / no DB).
// If any of these break, authentication is compromised.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{decode, DecodingKey, Validation};

    // A throwaway secret used only inside the test process. NOT a real credential.
    const TEST_SECRET: &str = "test-only-secret-not-a-real-key";

    fn decode_sub(token: &str, secret: &str) -> Result<String, jsonwebtoken::errors::Error> {
        let mut validation = Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.validate_aud = false;
        decode::<Claims>(
            token,
            &DecodingKey::from_secret(secret.as_bytes()),
            &validation,
        )
        .map(|d| d.claims.sub)
    }

    /// A freshly issued token must decode back to the same user id with the
    /// correct secret. This is the happy-path of every authenticated request.
    #[test]
    fn issued_token_round_trips_to_same_user() {
        let uid = Uuid::new_v4();
        let token = issue_token(&uid, TEST_SECRET).expect("issue");
        let sub = decode_sub(&token, TEST_SECRET).expect("decode");
        assert_eq!(sub, uid.to_string());
    }

    /// A token signed with secret A must be rejected when validated with
    /// secret B. If this ever passes, anyone could forge sessions → auth bypass.
    #[test]
    fn token_signed_with_other_secret_is_rejected() {
        let uid = Uuid::new_v4();
        let token = issue_token(&uid, TEST_SECRET).expect("issue");
        let res = decode_sub(&token, "a-totally-different-secret");
        assert!(
            res.is_err(),
            "token validated under the wrong secret — auth bypass"
        );
    }

    /// A tampered token body must fail signature verification.
    #[test]
    fn tampered_token_is_rejected() {
        let uid = Uuid::new_v4();
        let token = issue_token(&uid, TEST_SECRET).expect("issue");
        // Flip the last character of the signature segment.
        let mut bytes: Vec<char> = token.chars().collect();
        if let Some(last) = bytes.last_mut() {
            *last = if *last == 'A' { 'B' } else { 'A' };
        }
        let tampered: String = bytes.into_iter().collect();
        assert!(
            decode_sub(&tampered, TEST_SECRET).is_err(),
            "tampered token accepted — signature not enforced"
        );
    }

    /// bcrypt verify must accept the correct password and reject a wrong one.
    /// A false positive here = anyone can log into any account.
    #[test]
    fn password_verify_accepts_correct_rejects_wrong() {
        let hash = hash_password("correct horse battery staple").expect("hash");
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("wrong password", &hash));
        // A malformed (non-bcrypt) hash must never authenticate, not panic.
        assert!(!verify_password("anything", "not-a-valid-bcrypt-hash"));
    }
}
```

## ⚠ Handoff — api crate + production content

- **`api` crate is unbuildable on `master`** (24 errors, red CI since
  2026-03-11). The CI `test` job is therefore scoped to `cargo test -p shared`.
- **An uncommitted ~2-month-old WIP** in the working tree is mid-fix on the
  `api` crate. **Whether to commit or discard it is the owner's call** — it was
  left untouched here.
- **Production (`misebanai.com`) is currently serving that WIP's landing page**,
  not `master`. The live `web/landing/index.html` carries the
  `enabler-analytics` tag and ~192 lines of landing improvements that exist
  **only in the uncommitted WIP**. `master`'s `web/landing/index.html` does
  **not** have them. Deploying `master` as-is via `Dockerfile.web` would
  **regress production** (drop the analytics tag + the landing work). For this
  reason `deploy.yml` has a `deploy-gate` job that **skips** the `deploy` and
  `verify` jobs (neutral — master stays green) whenever the checked-out
  `index.html` is missing the `enabler-analytics` tag, instead of shipping a
  regression. The deploy → verify chain only runs once that tag is on `master`.
- **Next step (owner):** decide on the WIP. Once it (or an equivalent fix
  landing the analytics tag + api build) is committed to `master`, the
  `deploy.yml` pipeline (test → deploy → verify) will go green end-to-end.
