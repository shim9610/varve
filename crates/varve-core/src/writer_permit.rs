//! Mechanical enforcement of writer poison checks — defect **shape B**.
//!
//! Shape B is *"guard bypass"*: a poison/validity flag is checked by a public
//! guard, but an internal method reachable from another module performs the
//! guarded operation without it. Round 12's F-05 was exactly that —
//! `VarveStreamWriter::append_prepared_chunk` was called straight from
//! `indexed.rs` and consulted no flag — and the crate held *three* independent
//! hand-written poison flags (`stream.rs`, `file.rs`, `layout.rs`), each with
//! its own guard and its own convention about which methods must call it.
//!
//! A local check inside the one bypassed method closes one caller. It does not
//! stop the next mutating method from being written without one, which is the
//! failure mode seven review rounds have demonstrated. So the flag is wrapped
//! instead, once, for the whole crate:
//!
//! * [`PoisonFlag`] is the **only** holder of the boolean. Its fields are
//!   private to this module, so no other file in the crate can read or set the
//!   flag except through the methods below.
//! * [`MutationPermit`] is a witness that the flag was observed clear. It has a
//!   private field and exactly one constructor, `PoisonFlag::issue`, so it
//!   cannot be fabricated anywhere else — not even inside the module that owns
//!   the writer.
//!
//! Every guarded operation takes a permit. A caller that skips the check has
//! nothing to pass and does not compile; a mutating method added next month
//! cannot be given anything to mutate through without first asking for one.
//!
//! # Which flag the witness speaks for
//!
//! The first cut of this module let *any* holder of a `PoisonFlag` mint a
//! permit: `PoisonFlag::healthy()` was a `const fn` (round 15 made it a
//! plain `fn`; see its documentation) and `permit` was
//! `pub(crate)`, so three lines anywhere in the crate —
//!
//! ```text
//! let decoy = PoisonFlag::healthy();
//! let permit = decoy.permit("stream")?;
//! writer.append_prepared_chunk(permit, bytes, records)   // compiled!
//! ```
//!
//! — laundered a witness out of a throwaway flag and drove a genuinely
//! poisoned writer straight through its own guard. The permit proved that *a*
//! flag had been checked, not that *the writer being mutated* had been. That is
//! shape B again, one level up, and it compiled cleanly.
//!
//! Two changes close it, both enforced by the compiler rather than by review:
//!
//! * the constructor is **private to this module**. `PoisonFlag::issue` has no
//!   visibility modifier at all, so `stream.rs`, `indexed.rs`, `file.rs` and
//!   `layout.rs` cannot mint a permit from any flag, decoy or otherwise. The
//!   only route out of this module is [`GuardedWriter::writer_permit`], which
//!   takes `&self` of the writer and reads *that writer's own* flag — there is
//!   no signature anywhere that turns a bare flag into a witness.
//! * the witness is **typed by the writer it speaks for**:
//!   `MutationPermit<VarveStreamWriter>` is a different type from
//!   `MutationPermit<VarveFile>`, so a permit taken from one writer cannot be
//!   spent on another kind of writer even by accident.
//!
//! What remains unenforced is *instance* identity: a permit taken from one
//! `VarveStreamWriter` would still be accepted by another. Producing one now
//! requires a second, genuinely healthy writer of the same type — i.e. a second
//! open file — rather than a stack-allocated `bool`, and every such writer's
//! flag really was checked. Binding the instance needs either a lifetime on the
//! permit (which conflicts with the `&mut self` mutating methods unless every
//! writer's fields are split into a borrow-disjoint inner struct) or invariant
//! brands threaded through the public types. Recorded as an open item.
//!
//! # The in-flight window
//!
//! `layout.rs` does not use the flag as a one-way poison: it marks a segment
//! write *in flight* before the body runs and clears the mark when the body
//! either succeeds or is fully rolled back, so only a failed rollback leaves
//! the writer refusing. That is modelled by [`PoisonFlag::begin_mutation`] /
//! [`PoisonFlag::end_mutation`] and the [`MutationInFlight`] token rather than
//! by letting anyone assign the boolean:
//!
//! * `begin_mutation` consumes a permit, so an in-flight window cannot be
//!   opened on a writer that is already refusing.
//! * `end_mutation` consumes the token, so the mark cannot be cleared by code
//!   that never set it.
//! * the in-flight mark and the one-way poison are **separate fields**, so
//!   ending a window can never clear a real poison raised during it.
//!
//! # Cost
//!
//! [`MutationPermit`] and [`MutationInFlight`] are zero-sized. `PoisonFlag` is
//! two bytes where a single `bool` used to be. Taking a permit is one branch on
//! a value already in cache; nothing allocates and nothing syscalls, so the
//! continuous-append hot path is unaffected (invariant 1).

use core::fmt;
use core::marker::PhantomData;

use crate::{Error, Result};

/// A writer whose mutations are gated on a [`PoisonFlag`].
///
/// Implementing this is what makes a writer's own flag reachable as a source of
/// permits, and it is the **only** such source: `PoisonFlag::issue` is private
/// to this module, so no amount of code elsewhere in the crate can turn a flag
/// it happens to hold into a witness. [`Self::writer_permit`] takes `&self`, so
/// the flag it reads is the flag of the writer the permit will be spent on.
pub trait GuardedWriter: Sized {
    /// This writer's poison state. Returning a shared reference is harmless:
    /// nothing outside this module can mint a permit from it.
    fn poison_flag(&self) -> &PoisonFlag;

    /// Checks *this writer's* flag and, when clear, issues the witness its
    /// guarded operations demand.
    ///
    /// `context` names the writer the caller speaks for, so an indexed writer
    /// built on a stream reports `WriterPoisoned("indexed")` while still
    /// reading the one authoritative flag.
    fn writer_permit(&self, context: &'static str) -> Result<MutationPermit<Self>> {
        self.poison_flag().issue(context)
    }
}

/// Witness that a writer's poison flag was observed clear.
///
/// Held by value for one mutation where another module can reach the guarded
/// operation, so each such mutation is preceded by its own fresh check rather
/// than by one check amortised over a loop; held by reference inside a
/// mutation that is already permitted, where demanding a fresh one would
/// re-check a flag the same call may be about to set.
///
/// The permit deliberately carries no lifetime tied to the flag: binding one
/// would make staleness unrepresentable but conflicts with `&mut self` methods
/// without splitting every writer's fields into a borrow-disjoint inner
/// struct. See the open item in `docs/invariant-checklist.md`.
///
/// `W` names the writer the witness speaks for. It is phantom — the permit is
/// still zero-sized — but it means a permit taken from one kind of writer
/// cannot be spent on another, and it makes the guarded signatures state which
/// flag must have been read.
#[must_use = "a mutation permit is the proof that the poison check ran; \
              drop it only if the mutation is abandoned"]
pub struct MutationPermit<W: ?Sized>(PhantomData<fn() -> W>);

/// Witness that a reversible in-flight window is open on a writer.
///
/// Only [`PoisonFlag::begin_mutation`] produces one and only
/// [`PoisonFlag::end_mutation`] consumes one, so a path that returns without
/// ending the window leaves the writer refusing every later mutation — the
/// safe direction. `W` is carried over from the permit that opened the window,
/// so the token cannot be handed to a different writer's body.
#[must_use = "an in-flight window that is never ended leaves the writer refusing"]
pub struct MutationInFlight<W: ?Sized>(PhantomData<fn() -> W>);

impl<W: ?Sized> fmt::Debug for MutationPermit<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MutationPermit")
    }
}

impl<W: ?Sized> fmt::Debug for MutationInFlight<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MutationInFlight")
    }
}

/// A writer's poison state, and the sole source of [`MutationPermit`].
#[derive(Debug)]
pub struct PoisonFlag {
    /// One-way. Set by [`Self::poison`] and never cleared.
    poisoned: bool,
    /// Reversible. Set by [`Self::begin_mutation`], cleared by
    /// [`Self::end_mutation`], and kept separate from `poisoned` so that
    /// ending a window cannot clear a real poison raised inside it.
    in_flight: bool,
}

impl PoisonFlag {
    /// A fresh, usable writer.
    ///
    /// Deliberately **not** a `const fn` (round 15). `issue` being private
    /// stops a decoy flag from minting a witness directly, but while this was
    /// `const` a `static DECOY: PoisonFlag = PoisonFlag::healthy();` could be
    /// declared anywhere in the crate and returned from a writer's own
    /// `GuardedWriter::poison_flag`, at which point the default
    /// `writer_permit` would read the decoy instead of the writer's flag and
    /// every guarded operation would be permitted on a poisoned writer. That
    /// spelling no longer compiles. It is a narrowing, not a closure: the
    /// remaining route is a leaked heap allocation or a second flag field on
    /// the writer, and both are caught by the source gate
    /// `enforcement_gates.rs::a_writers_poison_flag_accessor_returns_its_own_field`
    /// rather than by the compiler. Named here so a future round does not read
    /// silence as coverage.
    pub fn healthy() -> Self {
        Self {
            poisoned: false,
            in_flight: false,
        }
    }

    /// The sole constructor of [`MutationPermit`], and **private to this
    /// module on purpose**.
    ///
    /// While this was `pub(crate)` any line in the crate could mint a witness
    /// from a throwaway `PoisonFlag::healthy()` and spend it on a poisoned
    /// writer (see the module docs). With no visibility modifier the only way
    /// out is [`GuardedWriter::writer_permit`], which reads the flag of the
    /// writer the permit names.
    fn issue<W: ?Sized>(&self, context: &'static str) -> Result<MutationPermit<W>> {
        if self.is_refusing() {
            Err(Error::WriterPoisoned(context))
        } else {
            Ok(MutationPermit(PhantomData))
        }
    }

    /// Refuses every future mutation of this writer. Not reversible.
    pub(crate) fn poison(&mut self) {
        self.poisoned = true;
    }

    /// Whether this writer refuses mutation, for either reason.
    pub(crate) fn is_refusing(&self) -> bool {
        self.poisoned || self.in_flight
    }

    /// Opens a reversible in-flight window. Consumes the permit, so a window
    /// cannot be opened on a writer that is already refusing.
    pub fn begin_mutation<W: ?Sized>(&mut self, permit: MutationPermit<W>) -> MutationInFlight<W> {
        let MutationPermit(_) = permit;
        self.in_flight = true;
        MutationInFlight(PhantomData)
    }

    /// Closes a window opened by [`Self::begin_mutation`]. Consumes the token,
    /// so nothing can clear a mark it did not set, and touches only the
    /// in-flight field, so a one-way poison raised during the window survives.
    pub fn end_mutation<W: ?Sized>(&mut self, in_flight: MutationInFlight<W>) {
        let MutationInFlight(_) = in_flight;
        self.in_flight = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for a real writer: the permits in these tests are taken the
    /// only way any writer in the crate can take one.
    struct Writer {
        poison: PoisonFlag,
    }

    impl GuardedWriter for Writer {
        fn poison_flag(&self) -> &PoisonFlag {
            &self.poison
        }
    }

    impl Writer {
        fn healthy() -> Self {
            Self {
                poison: PoisonFlag::healthy(),
            }
        }
    }

    #[test]
    fn a_poisoned_flag_issues_no_permit() {
        let mut writer = Writer::healthy();
        assert!(writer.writer_permit("test").is_ok());
        writer.poison.poison();
        assert!(matches!(
            writer.writer_permit("test"),
            Err(Error::WriterPoisoned("test"))
        ));
        assert!(writer.poison.is_refusing());
    }

    #[test]
    fn an_open_in_flight_window_refuses_a_second_permit() {
        let mut writer = Writer::healthy();
        let permit = writer.writer_permit("test").expect("healthy");
        let in_flight = writer.poison.begin_mutation(permit);
        assert!(writer.poison.is_refusing());
        assert!(writer.writer_permit("test").is_err());
        writer.poison.end_mutation(in_flight);
        assert!(!writer.poison.is_refusing());
        assert!(writer.writer_permit("test").is_ok());
    }

    /// The property that makes `end_mutation` safe to expose alongside a
    /// one-way `poison`: closing the window does not resurrect a poisoned
    /// writer. Without the two-field split this test fails.
    #[test]
    fn ending_a_window_does_not_clear_a_poison_raised_inside_it() {
        let mut writer = Writer::healthy();
        let permit = writer.writer_permit("test").expect("healthy");
        let in_flight = writer.poison.begin_mutation(permit);
        writer.poison.poison();
        writer.poison.end_mutation(in_flight);
        assert!(writer.poison.is_refusing());
        assert!(writer.writer_permit("test").is_err());
    }

    /// **The in-crate bypass catalogue for this module** (round 15).
    ///
    /// Compiled from `file.rs` — a different module, with the crate privileges
    /// a real defect would have — and observed to fail:
    ///
    /// ```text
    /// let _: MutationPermit<VarveFile> = MutationPermit(PhantomData);
    /// //  error[E0603]: tuple struct constructor `MutationPermit` is private
    ///
    /// let _: MutationPermit<VarveFile> = PoisonFlag::healthy().issue("x")?;
    /// //  error[E0624]: method `issue` is private
    /// ```
    ///
    /// And from this module, the spelling that round 15 removed:
    ///
    /// ```text
    /// static DECOY: PoisonFlag = PoisonFlag::healthy();
    /// //  error[E0015]: cannot call non-const associated function
    /// //               `PoisonFlag::healthy` in statics
    /// ```
    ///
    /// The gap round 14 closed, re-asserted. A second, healthy `PoisonFlag` in
    /// scope — the decoy that `PoisonFlag::healthy()` made a three-line affair — cannot
    /// produce a witness for a poisoned writer, because the only constructor
    /// reads the writer's own flag. Restoring a `pub(crate)` constructor on
    /// `PoisonFlag` is what would let this assertion be circumvented, and the
    /// source gate in `crates/varve/tests/enforcement_gates.rs` fails if it is.
    #[test]
    fn a_static_decoy_flag_cannot_be_declared() {
        // `static DECOY: PoisonFlag = PoisonFlag::healthy();` is rejected —
        // `healthy` is no longer a `const fn`. This is the runtime half: a
        // decoy still exists as a local, and still cannot mint anything.
        let decoy = PoisonFlag::healthy();
        assert!(!decoy.is_refusing());
    }

    #[test]
    fn a_second_healthy_flag_cannot_speak_for_a_poisoned_writer() {
        let mut writer = Writer::healthy();
        writer.poison.poison();
        let decoy = PoisonFlag::healthy();
        assert!(!decoy.is_refusing());
        // `decoy.issue::<Writer>("test")` is the laundering probe. It is
        // reachable *here* only because this module is `writer_permit` itself;
        // in every other module of the crate it does not compile.
        assert!(writer.writer_permit("test").is_err());
    }
}
