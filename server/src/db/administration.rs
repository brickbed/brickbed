//! Administrative close and readiness operations.

use super::*;

impl Db {
    pub async fn close(self) -> Result<(), AppError> {
        self.slate
            .close()
            .await
            .map_err(|e| AppError::Internal(format!("Failed to close DB: {}", e)))
    }

    /// Perform a real engine read for readiness checks. In particular, a
    /// SlateDB writer that has been fenced by a newer client returns an error
    /// here instead of continuing to advertise itself as ready.
    pub async fn health_check(&self) -> Result<(), AppError> {
        self.slate
            .get(b"_meta:health")
            .await
            .map(|_| ())
            .map_err(|e| AppError::Internal(format!("Database health check failed: {}", e)))
    }
}
