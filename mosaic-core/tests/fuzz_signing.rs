//! Adversarial signature-verification audit for Mosaic's ed25519-signed
//! artifacts (`Change`, `Comment`, `Approval`, `Issue`, `IssueEvent`).
//!
//! Each test builds a VALID signed artifact, then performs a single mutation
//! and asserts that `verify()` returns `Err` (i.e. tampering is rejected).
//! Any case where `verify()` returns `Ok` after tampering is a CRITICAL
//! signature-bypass bug.
//!
//! The final group of tests probes the identity<->key binding (attack #5):
//! whether an attacker can keep a victim's `author` Identity (email) while
//! substituting their OWN `author_key`+`sig` and still pass `verify()`.

use mosaic_core::attestation::Attestation;
use mosaic_core::error::Error;
use mosaic_core::issues::{Issue, IssueBuilder, IssueEvent, IssueEventBuilder, IssueEventKind};
use mosaic_core::m1::change::{Change, ChangeBuilder, ChangeId, FileChange, FileKind, Tai64N};
use mosaic_core::m1::identity::Identity;
use mosaic_core::m1::signing::{Signature, SigningKey};
use mosaic_core::review::{Approval, ApprovalBuilder, Comment, CommentBuilder, Verdict};
use mosaic_core::Hash;

// Deterministic seeded RNG, copied verbatim from `mosaic-core/src/crdt.rs` tests.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 ^ (self.0 >> 33)
    }
}

fn random_sig(rng: &mut Rng) -> Signature {
    let mut bytes = [0u8; 64];
    for chunk in bytes.chunks_mut(8) {
        let v = rng.next_u64().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
    Signature::from_bytes(&bytes)
}

fn victim_author() -> Identity {
    Identity::human("victim@example.com", Some("Victim".into())).unwrap()
}

fn sample_file() -> FileChange {
    FileChange {
        path: "src/main.rs".into(),
        kind: FileKind::Text,
        patch: b"@@ -1 +1 @@".to_vec(),
        conflicts: Vec::new(),
    }
}

fn sample_change_id() -> ChangeId {
    ChangeId(Hash::of(b"sample-change"))
}

fn build_change(sk: SigningKey) -> Change {
    ChangeBuilder::new(victim_author(), sk)
        .ts(Tai64N(1_000, 7))
        .intent("legitimate intent")
        .file(sample_file())
        .build()
        .unwrap()
}

/// Helper asserting that verify() rejects (must NOT be Ok).
fn assert_rejected(result: Result<(), Error>, case: &str) {
    assert!(
        result.is_err(),
        "SIGNATURE BYPASS: tampering accepted for case `{case}` (verify returned Ok)"
    );
}

// ===========================================================================
// Attack #1: field tampering on a Change (sig + author_key left intact).
// Each tamper must invalidate the signature because every mutated field is
// part of the signed canonical bytes.
// ===========================================================================

#[test]
fn change_tamper_intent_rejected() {
    let mut c = build_change(SigningKey::generate());
    c.verify().unwrap();
    c.intent = Some("malicious intent".into());
    assert_rejected(c.verify(), "change.intent");
}

#[test]
fn change_tamper_body_path_rejected() {
    let mut c = build_change(SigningKey::generate());
    c.verify().unwrap();
    c.body[0].path = "src/evil.rs".into();
    assert_rejected(c.verify(), "change.body.path");
}

#[test]
fn change_tamper_body_patch_content_rejected() {
    let mut c = build_change(SigningKey::generate());
    c.verify().unwrap();
    c.body[0].patch = b"@@ malicious payload @@".to_vec();
    assert_rejected(c.verify(), "change.body.patch");
}

#[test]
fn change_tamper_body_kind_rejected() {
    let mut c = build_change(SigningKey::generate());
    c.verify().unwrap();
    c.body[0].kind = FileKind::Binary;
    assert_rejected(c.verify(), "change.body.kind");
}

#[test]
fn change_tamper_ts_rejected() {
    let mut c = build_change(SigningKey::generate());
    c.verify().unwrap();
    c.ts = Tai64N(9_999, 0);
    assert_rejected(c.verify(), "change.ts");
}

#[test]
fn change_tamper_deps_rejected() {
    let mut c = build_change(SigningKey::generate());
    c.verify().unwrap();
    c.deps.push(ChangeId(Hash::of(b"injected-dep")));
    assert_rejected(c.verify(), "change.deps");
}

#[test]
fn change_tamper_author_email_rejected() {
    let mut c = build_change(SigningKey::generate());
    c.verify().unwrap();
    // Change ONLY the author identity (email) — leave sig + author_key intact.
    c.author = Identity::human("attacker@example.com", Some("Victim".into())).unwrap();
    assert_rejected(c.verify(), "change.author (email)");
}

// ===========================================================================
// Attack #1 (cont.): Comment / Approval / Issue / IssueEvent field tampering.
// ===========================================================================

fn build_comment(sk: SigningKey) -> Comment {
    CommentBuilder::new(sample_change_id(), victim_author(), sk)
        .ts(Tai64N(10, 0))
        .body("looks good to me")
        .build()
        .unwrap()
}

#[test]
fn comment_tamper_body_rejected() {
    let mut c = build_comment(SigningKey::generate());
    c.verify().unwrap();
    c.body = "this is garbage, reject it".into();
    assert_rejected(c.verify(), "comment.body");
}

#[test]
fn comment_tamper_author_email_rejected() {
    let mut c = build_comment(SigningKey::generate());
    c.verify().unwrap();
    c.author = Identity::human("attacker@example.com", Some("Victim".into())).unwrap();
    assert_rejected(c.verify(), "comment.author (email)");
}

fn build_approval(sk: SigningKey) -> Approval {
    ApprovalBuilder::new(sample_change_id(), victim_author(), sk, Verdict::RequestedChanges)
        .ts(Tai64N(20, 0))
        .body("needs work")
        .build()
        .unwrap()
}

#[test]
fn approval_tamper_verdict_rejected() {
    let mut a = build_approval(SigningKey::generate());
    a.verify().unwrap();
    // Flip RequestedChanges -> Approved: the dangerous tamper.
    a.verdict = Verdict::Approved;
    assert_rejected(a.verify(), "approval.verdict");
}

fn build_issue(sk: SigningKey) -> Issue {
    IssueBuilder::new(1, victim_author(), sk)
        .ts(Tai64N(30, 0))
        .title("real title")
        .body("real body")
        .build()
        .unwrap()
}

#[test]
fn issue_tamper_title_rejected() {
    let mut i = build_issue(SigningKey::generate());
    i.verify().unwrap();
    i.title = "malicious retitle".into();
    assert_rejected(i.verify(), "issue.title");
}

#[test]
fn issue_tamper_body_rejected() {
    let mut i = build_issue(SigningKey::generate());
    i.verify().unwrap();
    i.body = "malicious body".into();
    assert_rejected(i.verify(), "issue.body");
}

fn build_issue_event(sk: SigningKey) -> IssueEvent {
    IssueEventBuilder::new(
        1,
        victim_author(),
        sk,
        IssueEventKind::Comment {
            body: "i can repro".into(),
        },
    )
    .ts(Tai64N(40, 0))
    .build()
    .unwrap()
}

#[test]
fn issue_event_tamper_kind_rejected() {
    let mut e = build_issue_event(SigningKey::generate());
    e.verify().unwrap();
    e.kind = IssueEventKind::StatusChanged {
        to: mosaic_core::issues::IssueStatus::Closed,
    };
    assert_rejected(e.verify(), "issue_event.kind");
}

// ===========================================================================
// Attack #2: garbage / zero signature.
// ===========================================================================

#[test]
fn change_zero_signature_rejected() {
    let mut c = build_change(SigningKey::generate());
    c.verify().unwrap();
    c.sig = Signature::from_bytes(&[0u8; 64]);
    assert_rejected(c.verify(), "change zero-sig");
}

#[test]
fn change_random_signature_rejected() {
    let mut rng = Rng::new(0xDEADBEEF);
    for i in 0..16 {
        let mut c = build_change(SigningKey::generate());
        c.sig = random_sig(&mut rng);
        assert_rejected(c.verify(), &format!("change random-sig #{i}"));
    }
}

#[test]
fn comment_zero_signature_rejected() {
    let mut c = build_comment(SigningKey::generate());
    c.sig = Signature::from_bytes(&[0u8; 64]);
    assert_rejected(c.verify(), "comment zero-sig");
}

#[test]
fn issue_random_signature_rejected() {
    let mut rng = Rng::new(0x1234_5678);
    let mut i = build_issue(SigningKey::generate());
    i.sig = random_sig(&mut rng);
    assert_rejected(i.verify(), "issue random-sig");
}

// ===========================================================================
// Attack #3: cross-artifact signature swap (same key, two different changes).
// ===========================================================================

#[test]
fn change_cross_artifact_sig_swap_rejected() {
    let sk = SigningKey::generate();
    let a = ChangeBuilder::new(victim_author(), sk.clone())
        .ts(Tai64N(1, 0))
        .intent("change A")
        .file(sample_file())
        .build()
        .unwrap();
    let mut b = ChangeBuilder::new(victim_author(), sk)
        .ts(Tai64N(2, 0))
        .intent("change B")
        .file(sample_file())
        .build()
        .unwrap();
    a.verify().unwrap();
    b.verify().unwrap();

    // Put A's signature onto B (B keeps its own author_key == same key).
    b.sig = a.sig;
    assert_rejected(b.verify(), "change cross-artifact sig swap");
}

// ===========================================================================
// Attack #4: wrong-key signature — sign canonical bytes with K2 but leave
// author_key = K1. Signature is valid w.r.t. K2 but not w.r.t. the embedded
// author_key (K1), so it must fail.
// ===========================================================================

#[test]
fn change_wrong_key_signature_rejected() {
    let k1 = SigningKey::generate();
    let k2 = SigningKey::generate();
    let c = build_change(k1.clone());
    c.verify().unwrap();

    // Re-sign the exact canonical bytes with K2, but keep author_key = K1.
    let msg = c.canonical_bytes();
    let mut forged = c;
    forged.sig = k2.sign(&msg);
    // author_key is still K1's key.
    assert_rejected(forged.verify(), "change wrong-key signature (sig=K2, author_key=K1)");
}

// ===========================================================================
// Attack #5: re-attribution / forgery (THE IMPORTANT ONE).
//
// An attacker takes the victim's Change CONTENT and re-signs it with the
// ATTACKER's OWN key, setting BOTH sig AND author_key to the attacker's,
// while keeping author = "victim@example.com".
//
// We test the actual behavior and document it. If verify() accepts this,
// the email identity is NOT bound to any key at the verify() layer -> a
// real identity-spoofing gap.
// ===========================================================================

#[test]
fn change_reattribution_with_victim_email_documents_binding_gap() {
    // Victim's legitimate change.
    let victim_key = SigningKey::generate();
    let original = build_change(victim_key);
    original.verify().unwrap();

    // Attacker keeps the victim's author Identity (email) and the same content,
    // but substitutes their OWN author_key and re-signs with their OWN key.
    let attacker_key = SigningKey::generate();
    let attacker_pub = attacker_key.verifying_key();

    let mut forged = Change {
        author: original.author.clone(), // STILL "victim@example.com"
        ts: original.ts,
        deps: original.deps.clone(),
        intent: original.intent.clone(),
        body: original.body.clone(),
        sig: original.sig,         // placeholder, replaced below
        author_key: attacker_pub,  // attacker's key
    };
    let msg = forged.canonical_bytes();
    forged.sig = attacker_key.sign(&msg);

    let verify_result = forged.verify();

    // Document the outcome precisely.
    assert_eq!(
        forged.author.id(),
        "human:victim@example.com",
        "author Identity is still the victim's email"
    );
    assert_ne!(
        forged.author_key.to_bytes(),
        original.author_key.to_bytes(),
        "author_key was swapped to the attacker's key"
    );

    if verify_result.is_ok() {
        // EXPECTED per the design: sig matches author_key, and verify() does
        // NOT bind the email to any key. This is an identity-spoofing gap at
        // the verify() layer.
        eprintln!(
            "FINDING (attack #5): Change::verify() ACCEPTS a forged change whose \
             author = victim@example.com but author_key/sig = attacker's. The email \
             identity is NOT bound to a key by verify()."
        );
    } else {
        eprintln!(
            "Change::verify() rejected the re-attribution — email IS bound to a key \
             at the verify layer."
        );
    }
}

/// Probe whether the attestation layer (the only identity<->key binding in the
/// codebase) can catch the human-impersonation forgery from attack #5.
///
/// `Attestation` only supports `Identity::Agent`; it rejects `Identity::Human`.
/// So a Human-authored forged change cannot even be authorized/checked by it.
#[test]
fn attestation_layer_does_not_cover_human_impersonation() {
    let victim_key = SigningKey::generate();
    let original = build_change(victim_key);

    let attacker_key = SigningKey::generate();
    let mut forged = Change {
        author: original.author.clone(), // human victim
        ts: original.ts,
        deps: original.deps.clone(),
        intent: original.intent.clone(),
        body: original.body.clone(),
        sig: original.sig,
        author_key: attacker_key.verifying_key(),
    };
    let msg = forged.canonical_bytes();
    forged.sig = attacker_key.sign(&msg);

    // Try to issue an attestation for this (human) author — it must fail,
    // proving the attestation layer cannot bind a human email to a key.
    let att = Attestation::issue(
        forged.author.clone(),
        forged.author_key,
        Tai64N(0, 0),
        Tai64N(u64::MAX, 0),
        &attacker_key,
    );
    assert!(
        matches!(att, Err(Error::AttestationInvalidAgent)),
        "attestation layer only supports Agent identities; humans are unguarded"
    );
}

/// For completeness: the attacker can equally well claim the email belongs to
/// them and forge a brand-new authored change. verify() only checks the email
/// is non-empty (Identity::validate) and that sig matches author_key.
#[test]
fn freshly_forged_change_under_victim_email_documents_binding_gap() {
    let attacker_key = SigningKey::generate();
    let forged = ChangeBuilder::new(
        Identity::human("victim@example.com", Some("Victim".into())).unwrap(),
        attacker_key,
    )
    .ts(Tai64N(123, 0))
    .intent("backdoor")
    .file(FileChange {
        path: "src/auth.rs".into(),
        kind: FileKind::Text,
        patch: b"// disable auth".to_vec(),
        conflicts: Vec::new(),
    })
    .build()
    .unwrap();

    let r = forged.verify();
    if r.is_ok() {
        eprintln!(
            "FINDING (attack #5): a change authored under victim@example.com but \
             signed by the attacker's key passes verify(). No email<->key binding."
        );
    }
    // We assert the documented behavior so the test's verdict is explicit.
    assert!(
        r.is_ok(),
        "documenting that verify() does not bind email to key"
    );
}
