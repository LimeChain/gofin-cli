//! Optional performance state containing only public pricing identities.
//! Protected permissions prevent an agent replacing verified identity evidence.
use gofin_pricing::CoinGeckoPrices;

pub(super) fn attach(prices: CoinGeckoPrices) -> gofin_domain::Result<CoinGeckoPrices> {
    #[cfg(not(test))]
    {
        prices.with_identity_store(std::sync::Arc::new(ProtectedPricingCache))
    }
    #[cfg(test)]
    {
        Ok(prices)
    }
}

#[cfg(not(test))]
struct ProtectedPricingCache;
#[cfg(not(test))]
impl gofin_pricing::IdentityStore for ProtectedPricingCache {
    fn load(&self) -> gofin_domain::Result<Option<Vec<u8>>> {
        use std::io::Read;
        let path = std::path::Path::new("/Library/GoFin/private/pricing-identities.json");
        super::validate_private_file_parent(path)?;
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(cache_error()),
            Ok(_) => gofin_authorization::validate_metadata(path, true)?,
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|_| cache_error())?
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| cache_error())?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Ok(None);
        }
        Ok(Some(bytes))
    }
    fn save(&self, bytes: &[u8]) -> gofin_domain::Result<()> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        use std::{
            io::Write,
            sync::atomic::{AtomicU64, Ordering},
        };
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::path::Path::new("/Library/GoFin/private/pricing-identities.json");
        super::validate_private_file_parent(path)?;
        if std::fs::symlink_metadata(path).is_ok() {
            gofin_authorization::validate_metadata(path, true)?;
        }
        let temporary = path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let result = (|| {
            let mut file = options.open(&temporary).map_err(|_| cache_error())?;
            file.write_all(bytes).map_err(|_| cache_error())?;
            file.sync_all().map_err(|_| cache_error())?;
            std::fs::rename(&temporary, path).map_err(|_| cache_error())?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}
#[cfg(not(test))]
fn cache_error() -> gofin_domain::GoFinError {
    gofin_domain::GoFinError::new(
        "PRICE_CACHE_UNAVAILABLE",
        "valuation",
        "Protected public identity cache could not be read or saved.",
        "Check the protected GoFin installation; no credential or financial record was changed.",
    )
}
