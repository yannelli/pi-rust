//! Document watches: Chord's `Watch` with the Session's error type (spec section 9.2).

use pi_chord::delta::Op;
use pi_chord::{Context, Json, Watch};

use super::observation::CommittedObserver;
use crate::errors::DurableError;

/// A watch of one committed document incarnation: its value is the document. A retirement delivers `[["r", null]]`
/// with value `null`, then ends the watch as `retired`; the Session's close ends it as `ended`, its failure as
/// `failed` with the error that failed it.
pub type DocumentWatch = Watch<DurableError>;

impl CommittedObserver for Watch<DurableError> {
    fn advance(&self, value: Json, ops: Vec<Op>, context: Context) {
        Watch::advance(self, value, ops, context);
    }

    fn retire(&self, context: Context) {
        Watch::retire(self, context);
    }

    fn end(&self, failure: Option<DurableError>) {
        Watch::end(self, failure);
    }

    fn keeps_cancellation(&self) -> bool {
        true
    }
}
