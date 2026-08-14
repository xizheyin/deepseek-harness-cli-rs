/// Opaque operating-system entropy failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EntropyError;

type FillFn = fn(&mut [u8]) -> Result<(), EntropyError>;

/// Small instance-owned entropy boundary shared by production ID owners.
#[derive(Clone, Copy)]
pub(crate) struct EntropySource {
    fill: FillFn,
}

impl std::fmt::Debug for EntropySource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EntropySource")
            .field("configured", &true)
            .finish()
    }
}

impl EntropySource {
    pub(crate) const fn system() -> Self {
        Self { fill: system_fill }
    }

    #[cfg(test)]
    pub(crate) const fn injected(fill: FillFn) -> Self {
        Self { fill }
    }

    pub(crate) fn fill(&self, bytes: &mut [u8]) -> Result<(), EntropyError> {
        (self.fill)(bytes)
    }

    pub(crate) fn uuid_v4(&self) -> Result<uuid::Uuid, EntropyError> {
        let mut bytes = [0_u8; 16];
        self.fill(&mut bytes)?;
        Ok(uuid::Builder::from_random_bytes(bytes).into_uuid())
    }

    pub(crate) fn random_u128(&self) -> Result<u128, EntropyError> {
        let mut bytes = [0_u8; 16];
        self.fill(&mut bytes)?;
        Ok(u128::from_be_bytes(bytes))
    }
}

fn system_fill(bytes: &mut [u8]) -> Result<(), EntropyError> {
    getrandom::fill(bytes).map_err(|_| EntropyError)
}

#[cfg(test)]
mod tests {
    use super::{EntropyError, EntropySource};

    fn fixed(bytes: &mut [u8]) -> Result<(), EntropyError> {
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::try_from(index).unwrap_or(0);
        }
        Ok(())
    }

    fn failing(_bytes: &mut [u8]) -> Result<(), EntropyError> {
        Err(EntropyError)
    }

    #[test]
    fn injected_entropy_builds_v4_ids_and_full_width_samples() {
        let source = EntropySource::injected(fixed);
        let id = source.uuid_v4().unwrap();

        assert_eq!(id.get_version_num(), 4);
        assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
        assert_eq!(
            source.random_u128().unwrap(),
            u128::from_be_bytes([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,])
        );
    }

    #[test]
    fn injected_entropy_failure_is_opaque_and_non_panicking() {
        let source = EntropySource::injected(failing);

        assert_eq!(source.uuid_v4(), Err(EntropyError));
        assert_eq!(source.random_u128(), Err(EntropyError));
        assert_eq!(format!("{EntropyError:?}"), "EntropyError");
    }
}
