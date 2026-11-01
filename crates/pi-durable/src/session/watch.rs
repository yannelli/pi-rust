//! Document watches: Chord's `Watch` with the Session's error type (spec section 9.2).

use pi_chord::{Context, Watch};

use super::observation::CommittedObserver;
use super::{WatchCall, WatchEnd};
use crate::errors::DurableError;

/// A watch of one document address (kind, scope, and key), from the incarnation found at acquisition on. A retirement
/// is a call with `retired` set and the last value; a document created at the address later arrives as
/// `[["r", value]]`. It ends `ended` when its task ends (a task document), or at Session close; `failed` on failure.
pub type DocumentWatch = Watch<DurableError>;

impl CommittedObserver for Watch<DurableError> {
    fn advance(&self, call: WatchCall, context: Context) {
        Watch::advance(self, call, context);
    }

    fn end(&self, end: WatchEnd) {
        Watch::end(self, end);
    }

    /// A task document's watch delivers the retirement that ends its task, then ends.
    fn owner_ended(&self) {
        self.end_after_delivery();
    }

    fn keeps_cancellation(&self) -> bool {
        true
    }
}
