use storage_rocksdb::RocksDb;

use crate::config::Backup;

use super::AppResult;

pub async fn backup(options: Backup) -> AppResult<()> {
    let storage = RocksDb::open(&options.database.database_path)?;
    let info = storage.create_backup(&options.backup_path).await?;
    tracing::info!(
        backup_id = info.backup_id,
        files = info.file_count,
        "RocksDB backup verified"
    );
    Ok(())
}
