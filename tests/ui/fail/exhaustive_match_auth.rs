//! FAIL: an external crate CANNOT match `AuthKind` exhaustively without a
//! wildcard arm — the enum is `#[non_exhaustive]`, so a `_` arm is mandatory. If
//! this ever compiles, `AuthKind` has lost `#[non_exhaustive]` and adding a
//! variant silently becomes a MAJOR break
//! (docs/SEMVER.md#provider-port-versioning).

use chainview::AuthKind;

fn describe(auth: AuthKind) -> &'static str {
    match auth {
        AuthKind::None => "none",
        AuthKind::Token => "token",
        AuthKind::KeySecret => "key-secret",
        AuthKind::UserPass => "user-pass",
    }
}

fn main() {
    let _ = describe(AuthKind::None);
}
