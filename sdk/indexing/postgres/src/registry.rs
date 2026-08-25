//! Durable address selection, stored in `payment_wallets`.

use indexing::{
    AddressFilter, BlockHeight, BoxFuture, CanonicalAddress, IndexError, IndexErrorKind,
    IndexScope, RegisteredAddress, Registry,
};
use tokio_postgres::Row;

use crate::{Repository, prepare, row};

const REGISTER: &str = "\
INSERT INTO payment_wallets (id, chain, network, address, start_height, secret)
VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING";

const REGISTERED: &str = "\
SELECT id, address, start_height, secret FROM payment_wallets
WHERE chain = $1 AND network = $2 ORDER BY created_at, id";

impl Repository {
    async fn write_registration(&self, entry: RegisteredAddress) -> Result<(), IndexError> {
        self.check_scope(&entry.filter.address.scope)?;
        let height = row::as_i64(entry.filter.start_height.0, "start height")?;
        let client = self.client().await?;
        let statement = prepare(&client, REGISTER).await?;
        let written = client
            .execute(
                &statement,
                &[
                    &entry.id,
                    &self.scope.chain.0,
                    &self.scope.network,
                    &entry.filter.address.value,
                    &height,
                    &entry.material,
                ],
            )
            .await
            .map_err(crate::store)?;
        if written == 0 {
            // Either the identity or the (scope, address) pair already exists.
            // Replacing it silently could move an address birthday under a
            // checkpoint that was built without it.
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "an address is already registered for this identity or scope",
                false,
            ));
        }
        Ok(())
    }

    async fn read_registrations(
        &self,
        scope: &IndexScope,
    ) -> Result<Vec<RegisteredAddress>, IndexError> {
        self.check_scope(scope)?;
        let client = self.client().await?;
        let statement = prepare(&client, REGISTERED).await?;
        let rows = client
            .query(&statement, &[&scope.chain.0, &scope.network])
            .await
            .map_err(crate::store)?;
        rows.iter().map(|entry| registered(scope, entry)).collect()
    }
}

impl Registry for Repository {
    fn register<'a>(&'a self, entry: RegisteredAddress) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move { self.write_registration(entry).await })
    }

    fn registered<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Vec<RegisteredAddress>, IndexError>> {
        Box::pin(async move { self.read_registrations(scope).await })
    }
}

fn registered(scope: &IndexScope, entry: &Row) -> Result<RegisteredAddress, IndexError> {
    let start: i64 = entry.try_get("start_height").map_err(crate::store)?;
    Ok(RegisteredAddress {
        id: entry.try_get("id").map_err(crate::store)?,
        filter: AddressFilter {
            address: CanonicalAddress {
                scope: scope.clone(),
                value: entry.try_get("address").map_err(crate::store)?,
            },
            start_height: BlockHeight(
                u64::try_from(start).map_err(|_| row::store("stored start height is negative"))?,
            ),
        },
        material: entry.try_get("secret").map_err(crate::store)?,
    })
}
