use ncore::rt::{BoxFuture, boxed};
use ncore_claudesub::{CredentialStore, Credentials};

use crate::accounts_file::AccountStore;

impl CredentialStore for AccountStore<Credentials> {
    fn load(&self) -> BoxFuture<'_, Result<Credentials, String>> {
        boxed(async move {
            self.file
                .credentials(&self.key)
                .ok_or_else(|| "account removed".to_owned())
        })
    }

    fn save<'a>(&'a self, credentials: &'a Credentials) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            self.file.replace(&self.key, credentials);
            Ok(())
        })
    }
}
