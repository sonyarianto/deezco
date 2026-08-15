use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;

use crate::api::DeezerApi;

/// Abstraction over the API needed by the login flow (enables testing).
trait Authenticator {
    /// Validate an ARL cookie; returns whether the login succeeded.
    async fn login_via_arl(&self, arl: &str) -> Result<bool>;
}

impl Authenticator for DeezerApi {
    async fn login_via_arl(&self, arl: &str) -> Result<bool> {
        DeezerApi::login_via_arl(self, arl).await
    }
}

/// Get the config directory for storing ARL
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("deezco")
}

async fn read_stored_arl_from(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join(".arl"))
        .await
        .ok()
        .map(|s| s.trim().to_string())
}

async fn save_arl_to(dir: &Path, arl: &str) -> Result<()> {
    fs::create_dir_all(dir)
        .await
        .context("Failed to create config dir")?;
    fs::write(dir.join(".arl"), arl.trim())
        .await
        .context("Failed to save ARL")?;
    Ok(())
}

async fn remove_arl_from(dir: &Path) -> Result<()> {
    let path = dir.join(".arl");
    if path.exists() {
        fs::remove_file(&path)
            .await
            .context("Failed to remove ARL")?;
    }
    Ok(())
}

/// Remove stored ARL
pub async fn remove_arl() -> Result<()> {
    remove_arl_from(&config_dir()).await
}

/// Attempt login with stored ARL, or prompt the user
pub async fn login(api: &DeezerApi) -> Result<bool> {
    login_with(api, &config_dir(), default_prompt).await
}

/// Interactive ARL prompt (requires a terminal)
fn default_prompt() -> Result<String> {
    println!("No stored login found — you need your Deezer ARL cookie to use deezco.\n");
    println!("How to get it:");
    println!("  1. Log in to https://www.deezer.com in your browser");
    println!("  2. Press F12 to open Developer Tools");
    println!("  3. Go to Application > Cookies > https://www.deezer.com");
    println!("  4. Copy the value of the 'arl' cookie\n");
    println!("It is stored locally and used to log you in on later runs.\n");

    dialoguer::Input::new()
        .with_prompt("Paste your ARL")
        .interact_text()
        .map_err(Into::into)
}

/// The login state machine: try the stored ARL, fall back to prompting.
async fn login_with<A: Authenticator>(
    api: &A,
    dir: &Path,
    prompt: impl Fn() -> Result<String>,
) -> Result<bool> {
    // Try stored ARL first
    if let Some(arl) = read_stored_arl_from(dir).await
        && !arl.is_empty()
    {
        match api.login_via_arl(&arl).await {
            Ok(true) => return Ok(true),
            _ => {
                eprintln!("Stored ARL is invalid, removing...");
                let _ = remove_arl_from(dir).await;
            }
        }
    }

    // Prompt for a new ARL
    let arl = prompt()?;
    let logged_in = api.login_via_arl(&arl).await?;
    if logged_in {
        save_arl_to(dir, &arl).await?;
        Ok(true)
    } else {
        eprintln!("Login failed. Invalid ARL.");
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct FakeAuth {
        results: Arc<Mutex<VecDeque<Result<bool>>>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl FakeAuth {
        fn new(results: Vec<Result<bool>>) -> Self {
            Self {
                results: Arc::new(Mutex::new(results.into())),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Authenticator for FakeAuth {
        async fn login_via_arl(&self, arl: &str) -> Result<bool> {
            self.calls.lock().unwrap().push(arl.to_string());
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(false))
        }
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("deezco-auth-test-{name}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ok_prompt(result: &'static str) -> impl Fn() -> Result<String> {
        move || Ok(result.to_string())
    }

    fn prompt_never() -> impl Fn() -> Result<String> {
        || -> Result<String> { panic!("prompt should not be called") }
    }

    #[tokio::test]
    async fn valid_stored_arl_logs_in_without_prompting() {
        let dir = TestDir::new("valid-stored");
        let auth = FakeAuth::new(vec![Ok(true)]);
        save_arl_to(dir.path(), "valid-arl").await.unwrap();

        let result = login_with(&auth, dir.path(), prompt_never()).await;

        assert!(result.unwrap());
        assert_eq!(auth.calls(), vec!["valid-arl"]);
        // The valid stored ARL is kept
        assert_eq!(
            read_stored_arl_from(dir.path()).await.as_deref(),
            Some("valid-arl")
        );
    }

    #[tokio::test]
    async fn invalid_stored_arl_is_removed_and_user_is_prompted() {
        let dir = TestDir::new("invalid-stored");
        // Stored validation fails, the freshly prompted one succeeds
        let auth = FakeAuth::new(vec![Ok(false), Ok(true)]);
        save_arl_to(dir.path(), "stale-arl").await.unwrap();

        let result = login_with(&auth, dir.path(), ok_prompt("fresh-arl")).await;

        assert!(result.unwrap());
        assert_eq!(auth.calls(), vec!["stale-arl", "fresh-arl"]);
        // The invalid stored ARL was replaced by the prompted one
        assert_eq!(
            read_stored_arl_from(dir.path()).await.as_deref(),
            Some("fresh-arl")
        );
    }

    #[tokio::test]
    async fn errored_stored_validation_removes_arl_and_prompts() {
        let dir = TestDir::new("errored-stored");
        let auth = FakeAuth::new(vec![Err(anyhow::anyhow!("network down")), Ok(true)]);
        save_arl_to(dir.path(), "stale-arl").await.unwrap();

        let result = login_with(&auth, dir.path(), ok_prompt("fresh-arl")).await;

        assert!(result.unwrap());
        assert_eq!(auth.calls(), vec!["stale-arl", "fresh-arl"]);
        assert_eq!(
            read_stored_arl_from(dir.path()).await.as_deref(),
            Some("fresh-arl")
        );
    }

    #[tokio::test]
    async fn missing_stored_arl_prompts_and_saves() {
        let dir = TestDir::new("missing-stored");
        let auth = FakeAuth::new(vec![Ok(true)]);

        let result = login_with(&auth, dir.path(), ok_prompt("typed-arl")).await;

        assert!(result.unwrap());
        assert_eq!(auth.calls(), vec!["typed-arl"]);
        assert_eq!(
            read_stored_arl_from(dir.path()).await.as_deref(),
            Some("typed-arl")
        );
    }

    #[tokio::test]
    async fn empty_stored_arl_falls_back_to_prompt() {
        let dir = TestDir::new("empty-stored");
        let auth = FakeAuth::new(vec![Ok(true)]);
        save_arl_to(dir.path(), "   ").await.unwrap(); // trims to empty

        let result = login_with(&auth, dir.path(), ok_prompt("typed-arl")).await;

        assert!(result.unwrap());
        // The empty stored ARL is not validated
        assert_eq!(auth.calls(), vec!["typed-arl"]);
        assert_eq!(
            read_stored_arl_from(dir.path()).await.as_deref(),
            Some("typed-arl")
        );
    }

    #[tokio::test]
    async fn invalid_prompted_arl_returns_false_without_saving() {
        let dir = TestDir::new("invalid-prompted");
        let auth = FakeAuth::new(vec![Ok(false)]);

        let result = login_with(&auth, dir.path(), ok_prompt("bad-arl")).await;

        assert!(!result.unwrap());
        assert_eq!(auth.calls(), vec!["bad-arl"]);
        assert_eq!(read_stored_arl_from(dir.path()).await, None);
    }

    #[tokio::test]
    async fn prompt_error_propagates() {
        let dir = TestDir::new("prompt-error");
        let auth = FakeAuth::new(vec![]);

        let result = login_with(&auth, dir.path(), || -> Result<String> {
            Err(anyhow::anyhow!("no tty"))
        })
        .await;

        assert!(result.is_err());
        assert!(auth.calls().is_empty());
    }

    #[tokio::test]
    async fn save_and_read_arl_roundtrip() {
        let dir = TestDir::new("roundtrip");
        assert_eq!(read_stored_arl_from(dir.path()).await, None);

        save_arl_to(dir.path(), "  abc  ").await.unwrap();

        // Stored value is trimmed
        assert_eq!(
            read_stored_arl_from(dir.path()).await.as_deref(),
            Some("abc")
        );
    }

    #[tokio::test]
    async fn remove_arl_deletes_stored_arl_and_is_idempotent() {
        let dir = TestDir::new("remove");
        save_arl_to(dir.path(), "abc").await.unwrap();
        assert_eq!(
            read_stored_arl_from(dir.path()).await.as_deref(),
            Some("abc")
        );

        remove_arl_from(dir.path()).await.unwrap();
        assert_eq!(read_stored_arl_from(dir.path()).await, None);

        // Removing again is a no-op, not an error
        remove_arl_from(dir.path()).await.unwrap();
    }
}
