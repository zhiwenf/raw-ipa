use crate::error::BoxError;
use crate::ff::Field;
use crate::protocol::context::ProtocolContext;
use crate::protocol::RecordId;
use crate::secret_sharing::{MaliciousReplicated, Replicated, SecretShare};
use async_trait::async_trait;

mod malicious;
mod semi_honest;

/// Trait to multiply secret shares. That requires communication and `multiply` function is async.
#[async_trait]
#[allow(clippy::module_name_repetitions)]
pub trait SecureMul<F: Field> {
    type Share: SecretShare<F>;

    /// Multiply and return the result of `a` * `b`.
    async fn multiply(
        self,
        record_id: RecordId,
        a: Self::Share,
        b: Self::Share,
    ) -> Result<Self::Share, BoxError>;
}

/// looks like clippy disagrees with itself on whether this attribute is useless or not.
#[allow(clippy::module_name_repetitions, clippy::useless_attribute)]
pub use {malicious::SecureMul as MaliciouslySecureMul, semi_honest::SecureMul as SemiHonestMul};

/// Implement secure multiplication for semi-honest contexts with replicated secret sharing.
#[async_trait]
impl<F: Field> SecureMul<F> for ProtocolContext<'_, Replicated<F>, F> {
    type Share = Replicated<F>;

    async fn multiply(
        self,
        record_id: RecordId,
        a: Self::Share,
        b: Self::Share,
    ) -> Result<Self::Share, BoxError> {
        SemiHonestMul::new(self, record_id).execute(a, b).await
    }
}

/// Implement secure multiplication for malicious contexts with replicated secret sharing.
#[async_trait]
impl<F: Field> SecureMul<F> for ProtocolContext<'_, MaliciousReplicated<F>, F> {
    type Share = MaliciousReplicated<F>;

    async fn multiply(
        self,
        record_id: RecordId,
        a: Self::Share,
        b: Self::Share,
    ) -> Result<Self::Share, BoxError> {
        let acc = self.accumulator();
        MaliciouslySecureMul::new(self, record_id, acc)
            .execute(a, b)
            .await
    }
}
