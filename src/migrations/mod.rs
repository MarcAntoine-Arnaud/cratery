/*******************************************************************************
 * Copyright (c) 2024 Cénotélie Opérations SAS (cenotelie.fr)
 ******************************************************************************/

//! Module for the migrations of the platform database

use std::ops::DerefMut;

use log::info;
use sqlx::{Executor, SqliteConnection};

use crate::utils::apierror::ApiError;
use crate::utils::db::{in_transaction, Migration, MigrationContent, MigrationError, VersionNumber, SCHEMA_METADATA_VERSION};

/// The migrations
const MIGRATIONS: &[Migration<'static>] = &[
    Migration {
        target: "1.1.0",
        content: MigrationContent::Sql(include_bytes!("v1.1.0.sql")),
    },
    Migration {
        target: "1.2.0",
        content: MigrationContent::Sql(include_bytes!("v1.2.0.sql")),
    },
    Migration {
        target: "1.3.0",
        content: MigrationContent::Sql(include_bytes!("v1.3.0.sql")),
    },
    Migration {
        target: "1.4.0",
        content: MigrationContent::Sql(include_bytes!("v1.4.0.sql")),
    },
    Migration {
        target: "1.5.0",
        content: MigrationContent::Sql(include_bytes!("v1.5.0.sql")),
    },
];

/// Gets the value for the metadata item
///
/// # Errors
///
/// Return a `sqlx::Error` when the connection fail
///
/// # Panics
///
/// Panics when the SQL queries cannot be built
async fn get_schema_metadata(connection: &mut SqliteConnection, name_input: &str) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query!("SELECT value FROM SchemaMetadata WHERE name = $1 LIMIT 1", name_input)
        .fetch_optional(connection)
        .await?;
    Ok(row.map(|row| row.value))
}

/// Sets the value of a metadata item
///
/// # Errors
///
/// Return a `sqlx::Error` when the connection fail
///
/// # Panics
///
/// Panics when the SQL queries cannot be built
#[allow(clippy::explicit_deref_methods)]
async fn set_schema_metadata(mut connection: &mut SqliteConnection, n: &str, v: &str) -> Result<(), sqlx::Error> {
    let row = sqlx::query!("SELECT value FROM SchemaMetadata WHERE name = $1 LIMIT 1", n)
        .fetch_optional(connection.deref_mut())
        .await?;
    if row.is_none() {
        // insert new
        sqlx::query!("INSERT INTO SchemaMetadata (name, value) VALUES ($1, $2)", n, v)
            .execute(connection)
            .await?;
    } else {
        // update
        sqlx::query!("UPDATE SchemaMetadata SET value = $2 WHERE name = $1", n, v)
            .execute(connection)
            .await?;
    }
    Ok(())
}

/// The SQL to create the metadata table
const CREATE_METADATA_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS SchemaMetadata (
    name TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS SchemaMetadataIndex ON SchemaMetadata(name);";

/// Migrates a database to the last version
/// We assume that the connection is not already within a transaction
///
/// # Errors
///
/// Return a `MigrationError` when migration fails
async fn migrate_db(connection: &mut SqliteConnection, migrations: &[Migration<'_>]) -> Result<(), MigrationError> {
    let current_version = match get_schema_metadata(connection, SCHEMA_METADATA_VERSION).await {
        Ok(Some(version)) => Some(version),
        Ok(None) => None,
        _ => {
            // assume missing table => insert metadata table
            connection.execute(CREATE_METADATA_TABLE_SQL).await?;
            None
        }
    };
    let start_from = match current_version {
        Some(version) => {
            info!("Database schema version = {}", version);
            let version: VersionNumber = version.as_str().try_into()?;
            let mut result = 0;
            for (index, migration) in migrations.iter().enumerate().rev() {
                let target: VersionNumber = migration.target.try_into()?;
                if version >= target {
                    result = index + 1;
                    break;
                }
            }
            result
        }
        None => 0,
    };
    if start_from >= migrations.len() {
        return Ok(());
    }
    for migration in &migrations[start_from..] {
        info!("Database migrating to {} ...", migration.target);
        in_transaction(connection, |transaction| async move {
            match &migration.content {
                MigrationContent::Sql(script) => {
                    let script = String::from_utf8_lossy(script);
                    transaction.borrow().await.execute(script.as_ref()).await?;
                }
            }
            set_schema_metadata(&mut *transaction.borrow().await, SCHEMA_METADATA_VERSION, migration.target).await?;
            Ok::<_, MigrationError>(())
        })
        .await?;
    }
    info!("Database successfully migrated.");
    Ok(())
}

/// Migrate to the last version
pub async fn migrate_to_last(connection: &mut SqliteConnection) -> Result<i32, ApiError> {
    migrate_db(connection, MIGRATIONS).await?;
    Ok(0)
}
