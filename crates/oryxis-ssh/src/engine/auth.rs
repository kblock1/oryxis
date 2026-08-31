use super::*;

use russh::{MethodKind, MethodSet};

/// The login name a dial authenticates as.
///
/// One answer in one place, because it is read twice: the auth path
/// sends it, and a `ProxyCommand` line's `%r` names it. Two spellings of
/// the same fallback would let a proxy be told about a user the session
/// never logs in as.
pub(crate) fn effective_username(connection: &Connection) -> &str {
    connection.username.as_deref().unwrap_or("root")
}

impl SshEngine {
    // -----------------------------------------------------------------------
    // Authentication
    // -----------------------------------------------------------------------

    /// Authenticate on a handle (used for both direct and jump host connections).
    pub(crate) async fn authenticate_handle(
        &self,
        handle: &mut client::Handle<ClientHandler>,
        connection: &Connection,
        password: Option<&str>,
        key_material: Option<KeyMaterial<'_>>,
    ) -> Result<(), SshError> {
        let username = effective_username(connection);
        let has_pw = password.is_some();
        let has_key = key_material.is_some();
        tracing::info!(
            "Auth for {}@{} method={:?} has_password={} has_key={}",
            username, connection.hostname, connection.auth_method, has_pw, has_key,
        );

        match self
            .do_auth(handle, username, &connection.auth_method, password, key_material)
            .await
        {
            Ok(true) => {
                tracing::info!("Authenticated as {} on {}", username, connection.hostname);
                Ok(())
            }
            Ok(false) => Err(SshError::Key(format!(
                "Auth rejected for \"{}\" (method: {:?}, password: {}, key: {})",
                username, connection.auth_method, has_pw, has_key,
            ))),
            Err(e) => Err(e),
        }
    }

    pub(crate) async fn do_auth(
        &self,
        handle: &mut client::Handle<ClientHandler>,
        username: &str,
        auth_method: &AuthMethod,
        password: Option<&str>,
        key_material: Option<KeyMaterial<'_>>,
    ) -> Result<bool, SshError> {
        match auth_method {
            AuthMethod::Auto => {
                let mut tried: Vec<&str> = Vec::new();

                // 1. Try publickey if a key is provided
                if let Some(km) = key_material {
                    tried.push("publickey");
                    tracing::info!("Auto: trying publickey auth for {}", username);
                    match self.try_publickey_auth(handle, username, km).await {
                        Ok(StepVerdict::Accepted) => return Ok(true),
                        Ok(StepVerdict::Partial(remaining)) => {
                            return self
                                .finish_partial_auth(
                                    handle, username, remaining, None, password, password,
                                )
                                .await;
                        }
                        Ok(StepVerdict::Rejected) => tracing::info!("Auto: publickey rejected"),
                        Err(e) => tracing::info!("Auto: publickey error: {}", e),
                    }
                }

                // 2. Try agent auth
                tried.push("agent");
                tracing::info!("Auto: trying agent auth for {}", username);
                match self.auth_via_agent(handle, username).await {
                    Ok(AgentAuthOutcome::Authenticated) => return Ok(true),
                    Ok(AgentAuthOutcome::Partial(remaining)) => {
                        return self
                            .finish_partial_auth(
                                handle, username, remaining, None, password, password,
                            )
                            .await;
                    }
                    Ok(AgentAuthOutcome::NoMatch(tally)) => {
                        tracing::info!("Auto: agent had no matching keys ({tally})")
                    }
                    Err(e) => {
                        // A dead transport (server disconnected mid-
                        // sweep, e.g. MaxAuthTries) can't be salvaged
                        // by the next method: surface the real error
                        // instead of a misleading "all methods failed".
                        if handle.is_closed() {
                            return Err(e);
                        }
                        tracing::info!("Auto: agent unavailable: {}", e);
                    }
                }

                // 3. Try password if available
                if let Some(pw) = password {
                    tried.push("password");
                    tracing::info!("Auto: trying password auth for {}", username);
                    match handle.authenticate_password(username, pw).await {
                        Ok(res) => match res.into() {
                            StepVerdict::Accepted => return Ok(true),
                            // The key rejected (or skipped) at step 1 is
                            // re-offered: sshd only advertises the methods
                            // that are NEXT in an `AuthenticationMethods`
                            // list, so a `password,publickey` server refuses
                            // the out-of-order key first and wants it now.
                            StepVerdict::Partial(remaining) => {
                                return self
                                    .finish_partial_auth(
                                        handle, username, remaining, key_material, None, Some(pw),
                                    )
                                    .await;
                            }
                            StepVerdict::Rejected => tracing::info!("Auto: password rejected"),
                        },
                        Err(e) => tracing::info!("Auto: password error: {}", e),
                    }

                    // 4. Try keyboard-interactive with password. Silent
                    // (use_callback = false): it only reaches here after
                    // password already failed, so a prompt at the tail of a
                    // saved Auto host would be surprising. The user picks
                    // AuthMethod::Interactive when they want the modal; the
                    // quick-connect opt-in below is the one exception.
                    tried.push("keyboard-interactive");
                    tracing::info!("Auto: trying keyboard-interactive auth for {}", username);
                    match self.try_keyboard_interactive(handle, username, Some(pw), false).await? {
                        KbiOutcome::Success => return Ok(true),
                        // Same out-of-order rule as the password arm above:
                        // the step-1 key may be what the server wants next.
                        KbiOutcome::Partial(remaining) => {
                            return self
                                .finish_partial_auth(
                                    handle, username, remaining, key_material, None, Some(pw),
                                )
                                .await;
                        }
                        KbiOutcome::Rejected | KbiOutcome::Cancelled => {}
                    }
                }

                // 5. Quick-connect fallback (`with_auto_interactive_fallback`):
                // surface the interactive prompt the way OpenSSH would once
                // every silent method has failed, instead of erroring out.
                if self.auto_interactive_fallback && self.kbi_ask_tx.is_some() {
                    tried.push("keyboard-interactive (prompt)");
                    tracing::info!("Auto: trying prompted keyboard-interactive auth for {}", username);
                    match self.try_keyboard_interactive(handle, username, password, true).await? {
                        KbiOutcome::Success => return Ok(true),
                        KbiOutcome::Partial(remaining) => {
                            return self
                                .finish_partial_auth(
                                    handle, username, remaining, key_material, password, password,
                                )
                                .await;
                        }
                        // An explicit cancel ends the attempt; chaining a
                        // second modal after it would fight the user.
                        KbiOutcome::Cancelled => {
                            return Err(SshError::Key("Authentication cancelled".into()));
                        }
                        // The server may not offer keyboard-interactive at
                        // all (password-only sshd): one prompted password
                        // attempt before giving up.
                        KbiOutcome::Rejected => {
                            tried.push("password (prompt)");
                            tracing::info!("Auto: trying prompted password auth for {}", username);
                            match self.prompt_password_once(None).await {
                                Some(pw) => {
                                    let res = tokio::time::timeout(
                                        self.auth_timeout,
                                        handle.authenticate_password(username, &pw),
                                    )
                                    .await
                                    .map_err(|_| {
                                        SshError::ConnectionFailed(format!(
                                            "auth timed out after {}s",
                                            self.auth_timeout.as_secs()
                                        ))
                                    })??;
                                    match res.into() {
                                        StepVerdict::Accepted => return Ok(true),
                                        StepVerdict::Partial(remaining) => {
                                            return self
                                                .finish_partial_auth(
                                                    handle,
                                                    username,
                                                    remaining,
                                                    key_material,
                                                    None,
                                                    Some(&pw),
                                                )
                                                .await;
                                        }
                                        StepVerdict::Rejected => {}
                                    }
                                }
                                None => {
                                    return Err(SshError::Key(
                                        "Authentication cancelled".into(),
                                    ));
                                }
                            }
                        }
                    }
                }

                Err(SshError::Key(format!(
                    "All auto auth methods failed for \"{}\". Tried: {}",
                    username,
                    tried.join(", ")
                )))
            }
            AuthMethod::Password => {
                let pw = password.ok_or(SshError::AuthFailed)?;
                tracing::info!("Trying password auth for {}", username);
                let res = handle.authenticate_password(username, pw).await?;
                match res.into() {
                    StepVerdict::Accepted => Ok(true),
                    // The password was CORRECT; the server wants a second
                    // factor (TOTP over keyboard-interactive, issue #125).
                    StepVerdict::Partial(remaining) => {
                        self.finish_partial_auth(
                            handle,
                            username,
                            remaining,
                            key_material,
                            None,
                            Some(pw),
                        )
                        .await
                    }
                    StepVerdict::Rejected => {
                        Err(SshError::Key("Password rejected by server".into()))
                    }
                }
            }
            AuthMethod::Key => {
                let km = key_material
                    .ok_or_else(|| SshError::Key("No private key selected".into()))?;

                // Strictly the bare key (B2.1): the user picked "Key", so an
                // attached certificate is never offered here. `Certificate`
                // is the cert-only method and `Auto` the smart one.
                let km = KeyMaterial::plain(km.private_pem);

                tracing::info!("Trying publickey auth for {}", username);
                match self.try_publickey_auth(handle, username, km).await? {
                    StepVerdict::Accepted => return Ok(true),
                    // The key was CORRECT; the server wants another factor
                    // (key + TOTP, key + password, issue #125).
                    StepVerdict::Partial(remaining) => {
                        return self
                            .finish_partial_auth(
                                handle, username, remaining, None, password, password,
                            )
                            .await;
                    }
                    StepVerdict::Rejected => {}
                }

                // Key was rejected, try password as fallback if available
                if let Some(pw) = password {
                    tracing::info!("Key rejected, trying password fallback for {}", username);
                    let res = handle.authenticate_password(username, pw).await?;
                    match res.into() {
                        StepVerdict::Accepted => return Ok(true),
                        // Re-offer the rejected key: on a `password,publickey`
                        // server the step-1 refusal was order, not verdict.
                        StepVerdict::Partial(remaining) => {
                            return self
                                .finish_partial_auth(
                                    handle, username, remaining, Some(km), None, Some(pw),
                                )
                                .await;
                        }
                        StepVerdict::Rejected => {}
                    }
                    return Err(SshError::Key("Both key and password rejected by server".into()));
                }

                Err(SshError::Key("Public key rejected by server".into()))
            }
            AuthMethod::Certificate => {
                // Certificate-only (B2.1): offer the attached OpenSSH user
                // certificate and nothing else. Unlike the degrade-friendly
                // `try_publickey_auth`, everything here is a hard error: the
                // user asked for exactly this credential, so a missing or
                // unusable cert must surface instead of silently landing on
                // a different auth path.
                let km = key_material
                    .ok_or_else(|| SshError::Key("No key selected".into()))?;
                let cert_line = km.certificate.ok_or_else(|| {
                    SshError::Key("The selected key has no attached certificate".into())
                })?;

                let private_key = russh::keys::decode_secret_key(km.private_pem, None)
                    .map_err(|e| SshError::Key(format!("Failed to decode key: {}", e)))?;
                let private_key = Arc::new(private_key);

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let cert = match check_certificate(cert_line, &private_key, now) {
                    CertCheck::Unusable(why) => {
                        return Err(SshError::Key(format!("Certificate unusable: {}", why)));
                    }
                    CertCheck::Offer { cert, expired } => {
                        if expired {
                            // Advisory only: the server's clock is authoritative.
                            tracing::warn!(
                                "Certificate for {} is expired; offering anyway",
                                username,
                            );
                        }
                        cert
                    }
                };
                tracing::info!("Trying certificate auth for {}", username);
                let res = handle
                    .authenticate_openssh_cert(username, private_key, *cert)
                    .await?;
                match res.into() {
                    StepVerdict::Accepted => Ok(true),
                    StepVerdict::Partial(remaining) => {
                        self.finish_partial_auth(
                            handle, username, remaining, None, password, password,
                        )
                        .await
                    }
                    StepVerdict::Rejected => {
                        Err(SshError::Key("Certificate rejected by server".into()))
                    }
                }
            }
            AuthMethod::Agent => {
                tracing::info!("Trying agent auth for {}", username);
                match self.auth_via_agent(handle, username).await {
                    Ok(AgentAuthOutcome::Authenticated) => Ok(true),
                    // An agent key was accepted as the first factor; the
                    // server requires more (issue #125).
                    Ok(AgentAuthOutcome::Partial(remaining)) => {
                        self.finish_partial_auth(
                            handle, username, remaining, None, password, password,
                        )
                        .await
                    }
                    Ok(AgentAuthOutcome::NoMatch(tally)) => {
                        if let Some(pw) = password {
                            tracing::info!("Agent auth failed, trying password for {}", username);
                            let res = handle.authenticate_password(username, pw).await?;
                            match res.into() {
                                StepVerdict::Accepted => return Ok(true),
                                StepVerdict::Partial(remaining) => {
                                    return self
                                        .finish_partial_auth(
                                            handle, username, remaining, None, None, Some(pw),
                                        )
                                        .await;
                                }
                                StepVerdict::Rejected => {}
                            }
                        }
                        Err(SshError::Key(format!(
                            "Agent auth failed, no keys matched ({tally})"
                        )))
                    }
                    Err(e) => {
                        // A dead transport makes the password fallback
                        // pointless; surface the real error directly.
                        if handle.is_closed() {
                            return Err(e);
                        }
                        if let Some(pw) = password {
                            tracing::info!("Agent unavailable ({}), trying password for {}", e, username);
                            let res = handle.authenticate_password(username, pw).await?;
                            match res.into() {
                                StepVerdict::Accepted => return Ok(true),
                                StepVerdict::Partial(remaining) => {
                                    return self
                                        .finish_partial_auth(
                                            handle, username, remaining, None, None, Some(pw),
                                        )
                                        .await;
                                }
                                StepVerdict::Rejected => {}
                            }
                        }
                        Err(e)
                    }
                }
            }
            AuthMethod::Interactive => {
                tracing::info!("Trying keyboard-interactive auth for {}", username);
                match self.try_keyboard_interactive(handle, username, password, true).await? {
                    KbiOutcome::Success => Ok(true),
                    // The exchange was accepted; the server wants another
                    // method on top (issue #125).
                    KbiOutcome::Partial(remaining) => {
                        self.finish_partial_auth(
                            handle, username, remaining, key_material, password, password,
                        )
                        .await
                    }
                    // Rejection and cancel both surfaced the same error
                    // before the outcome split; keep that behavior.
                    KbiOutcome::Rejected | KbiOutcome::Cancelled => {
                        Err(SshError::Key("Keyboard-interactive auth rejected".into()))
                    }
                }
            }
            AuthMethod::PasswordPrompt => {
                // Ask the UI for the password (never stored). The human wait
                // is unbounded; only the network exchange below is capped so
                // a server wedging after the user types can't hang forever.
                let pw = self
                    .prompt_password_once(password)
                    .await
                    .ok_or_else(|| SshError::Key("Password entry cancelled".into()))?;
                tracing::info!("Trying prompted password auth for {}", username);
                let res = tokio::time::timeout(
                    self.auth_timeout,
                    handle.authenticate_password(username, &pw),
                )
                .await
                .map_err(|_| {
                    SshError::ConnectionFailed(format!(
                        "auth timed out after {}s",
                        self.auth_timeout.as_secs()
                    ))
                })??;
                match res.into() {
                    StepVerdict::Accepted => Ok(true),
                    StepVerdict::Partial(remaining) => {
                        self.finish_partial_auth(
                            handle,
                            username,
                            remaining,
                            key_material,
                            None,
                            Some(&pw),
                        )
                        .await
                    }
                    StepVerdict::Rejected => {
                        Err(SshError::Key("Password rejected by server".into()))
                    }
                }
            }
        }
    }

    /// Run the RFC 4252 partial-success continuation and convert "ran out
    /// of answerable methods" into the user-facing error, so every
    /// `do_auth` branch can `return self.finish_partial_auth(..)` after a
    /// partial success. `Ok(true)` is the only `Ok` this ever returns.
    #[allow(clippy::too_many_arguments)]
    async fn finish_partial_auth(
        &self,
        handle: &mut client::Handle<ClientHandler>,
        username: &str,
        remaining: MethodSet,
        unused_key: Option<KeyMaterial<'_>>,
        unused_password: Option<&str>,
        kbi_pw: Option<&str>,
    ) -> Result<bool, SshError> {
        tracing::info!(
            "Server accepted the first auth factor for {}, requires more: {:?}",
            username,
            remaining.iter().map(<&str>::from).collect::<Vec<_>>(),
        );
        if self
            .continue_partial_auth(handle, username, remaining, unused_key, unused_password, kbi_pw)
            .await?
        {
            return Ok(true);
        }
        Err(SshError::Key(format!(
            "The server accepted the first authentication factor for \"{}\" but requires \
             additional authentication (2FA) that could not be completed",
            username,
        )))
    }

    /// Drive an authentication the server accepted PARTIALLY (RFC 4252
    /// partial success: the offered factor was correct, more are
    /// required; sshd `AuthenticationMethods`, Bitvise compound auth,
    /// issue #125). Each round offers whichever remaining method still
    /// has an unconsumed answer, silent ones first:
    ///
    /// - `publickey` with `unused_key`;
    /// - `password` with `unused_password`;
    /// - `keyboard-interactive`, answered by the TOTP autofill, then the
    ///   UI modal, then `kbi_pw` (the `try_keyboard_interactive` order).
    ///
    /// The UI prompt is deliberately allowed here even for saved
    /// non-Interactive hosts: unlike the silent `Auto` tail (where a
    /// modal after a REJECTED password would be a surprise), the server
    /// has verified the first factor and explicitly demands another, so
    /// prompting is the only alternative to an unusable host. This is
    /// what OpenSSH does for `password,keyboard-interactive` servers.
    ///
    /// Returns `Ok(true)` on full authentication, `Ok(false)` once the
    /// server's remaining set carries nothing more we can answer. Each
    /// network exchange is individually bounded (callers outside the
    /// blanket auth timeout, e.g. `Interactive`, still park unbounded
    /// only on human input).
    async fn continue_partial_auth(
        &self,
        handle: &mut client::Handle<ClientHandler>,
        username: &str,
        mut remaining: MethodSet,
        mut unused_key: Option<KeyMaterial<'_>>,
        mut unused_password: Option<&str>,
        kbi_pw: Option<&str>,
    ) -> Result<bool, SshError> {
        // Backstop against a server that answers every step with yet
        // another partial success. Real chains are 2-3 factors deep.
        const MAX_STEPS: usize = 5;

        let net_timeout = self.auth_timeout;
        let net_err = || {
            SshError::ConnectionFailed(format!(
                "auth exchange timed out after {}s",
                net_timeout.as_secs()
            ))
        };

        // Keyboard-interactive is consumed by a REJECTED run (a second
        // one would re-ask, or autofill, a factor the server just
        // refused) and re-armed by a partially-accepted one (the server
        // wants another, distinct KBI factor next).
        let mut kbi_available = true;

        for _ in 0..MAX_STEPS {
            let verdict = if remaining.contains(&MethodKind::PublicKey) && unused_key.is_some() {
                let km = unused_key.take().expect("checked is_some");
                tracing::info!("Partial success: continuing with publickey for {}", username);
                tokio::time::timeout(
                    net_timeout,
                    self.try_publickey_auth(handle, username, km),
                )
                .await
                .map_err(|_| net_err())??
            } else if remaining.contains(&MethodKind::Password) && unused_password.is_some() {
                let pw = unused_password.take().expect("checked is_some");
                tracing::info!("Partial success: continuing with password for {}", username);
                let res = tokio::time::timeout(
                    net_timeout,
                    handle.authenticate_password(username, pw),
                )
                .await
                .map_err(|_| net_err())??;
                StepVerdict::from(res)
            } else if remaining.contains(&MethodKind::KeyboardInteractive) && kbi_available {
                kbi_available = false;
                tracing::info!(
                    "Partial success: continuing with keyboard-interactive for {}",
                    username
                );
                // Network rounds are bounded inside; the human wait (the
                // 2FA modal when no TOTP secret is stored) is not.
                match self.try_keyboard_interactive(handle, username, kbi_pw, true).await? {
                    KbiOutcome::Success => StepVerdict::Accepted,
                    // This exchange was ACCEPTED and the server asked for
                    // more: a follow-up keyboard-interactive is a new
                    // factor (`AuthenticationMethods
                    // keyboard-interactive,keyboard-interactive`), not a
                    // re-ask of a refused one, so re-arm it. MAX_STEPS
                    // still bounds the chain.
                    KbiOutcome::Partial(next) => {
                        kbi_available = true;
                        StepVerdict::Partial(next)
                    }
                    KbiOutcome::Rejected => StepVerdict::Rejected,
                    KbiOutcome::Cancelled => {
                        return Err(SshError::Key("Authentication cancelled".into()));
                    }
                }
            } else {
                return Ok(false);
            };
            match verdict {
                StepVerdict::Accepted => return Ok(true),
                // The server's follow-up set replaces ours wholesale;
                // it is authoritative about what may come next.
                StepVerdict::Partial(next) => remaining = next,
                // Rejection consumed the credential it was tried with;
                // the loop moves on to the next answerable method.
                StepVerdict::Rejected => {}
            }
        }
        tracing::warn!(
            "partial-success auth exceeded {} continuation steps, giving up",
            MAX_STEPS
        );
        Ok(false)
    }

    /// Ask the UI for a password once, for `AuthMethod::PasswordPrompt`.
    ///
    /// Sends a single-field, non-echoed prompt through `kbi_ask_tx` (the
    /// same bridge keyboard-interactive uses) and returns the typed value.
    /// Returns `None` when the user cancels or the UI bridge is gone.
    /// Headless callers (no `kbi_ask_tx`) fall back to `fallback_pw`, so
    /// MCP / boot port-forwards still authenticate without a modal.
    pub(crate) async fn prompt_password_once(&self, fallback_pw: Option<&str>) -> Option<String> {
        let Some(tx) = self.kbi_ask_tx.as_ref() else {
            return fallback_pw.map(|s| s.to_string());
        };
        let query = KbiQuery {
            name: self
                .pw_prompt_title
                .clone()
                .unwrap_or_else(|| "Enter Password".to_string()),
            instructions: String::new(),
            prompts: vec![KbiPromptField {
                prompt: self
                    .pw_prompt_label
                    .clone()
                    .unwrap_or_else(|| "Password".to_string()),
                echo: false,
            }],
        };
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        if tx.send((query, resp_tx)).await.is_err() {
            // UI bridge dropped: treat as cancellation.
            return None;
        }
        match resp_rx.await {
            Ok(Some(mut answers)) => answers.drain(..).next(),
            // User cancelled, or the responder was dropped.
            Ok(None) | Err(_) => None,
        }
    }

    /// Try publickey auth, signing RSA keys with the hash the server actually
    /// accepts (`server_rsa_hash`) so legacy `ssh-rsa` / SHA-1 servers still
    /// authenticate instead of the client insisting on rsa-sha2-256.
    pub(crate) async fn try_publickey_auth(
        &self,
        handle: &mut client::Handle<ClientHandler>,
        username: &str,
        material: KeyMaterial<'_>,
    ) -> Result<StepVerdict, SshError> {
        let private_key = russh::keys::decode_secret_key(material.private_pem, None)
            .map_err(|e| SshError::Key(format!("Failed to decode key: {}", e)))?;
        let private_key = Arc::new(private_key);

        // If a certificate is attached, offer it first. Anything wrong with
        // the cert itself (unparseable, not this key's cert) degrades to a
        // plain publickey attempt instead of failing the whole auth: a bad
        // cert must never brick a host that could still authenticate with
        // the bare key. This matters because `AuthMethod::Key` propagates a
        // returned `Err` (skipping its password fallback), so cert trouble
        // is signalled by falling through, never by `Err`. Only a decode or
        // transport failure is a real `Err` here.
        if let Some(cert_line) = material.certificate {
            match self
                .try_certificate_auth(handle, username, &private_key, cert_line)
                .await?
            {
                // Accepted outright, or accepted as a first factor with
                // more required: both end the publickey attempt (falling
                // through to the bare key after a partial success would
                // offer a credential the server did not ask for).
                Some(v @ (StepVerdict::Accepted | StepVerdict::Partial(_))) => return Ok(v),
                // Offered but the server rejected the cert, or the cert was
                // unusable: fall through to the bare key (OpenSSH treats
                // the cert and the plain key as separate identities).
                Some(StepVerdict::Rejected) | None => {
                    tracing::info!("Falling back to bare public key for {}", username);
                }
            }
        }

        // Plain publickey, signing RSA with the hash the server accepts.
        let hash = if private_key.algorithm().is_rsa() {
            server_rsa_hash(handle).await
        } else {
            None
        };
        let key = PrivateKeyWithHashAlg::new(private_key, hash);
        let res = handle.authenticate_publickey(username, key).await?;
        Ok(res.into())
    }

    /// Offer an OpenSSH certificate during publickey auth. Returns:
    /// - `Ok(Some(verdict))` the offer reached the server (accepted,
    ///   rejected, or accepted-partially per RFC 4252);
    /// - `Ok(None)` the cert is unusable (unparseable, or it does not certify
    ///   this key) so the caller should try the bare key;
    /// - `Err(..)` a transport failure (propagated like plain auth).
    ///
    /// Expiry is advisory only: the server's clock is authoritative, so an
    /// expired cert is logged as a warning and still offered.
    async fn try_certificate_auth(
        &self,
        handle: &mut client::Handle<ClientHandler>,
        username: &str,
        private_key: &Arc<russh::keys::PrivateKey>,
        cert_line: &str,
    ) -> Result<Option<StepVerdict>, SshError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cert = match check_certificate(cert_line, private_key, now) {
            CertCheck::Unusable(why) => {
                tracing::warn!("Attached certificate unusable ({why}); using bare key");
                return Ok(None);
            }
            CertCheck::Offer { cert, expired } => {
                if expired {
                    tracing::warn!(
                        "Certificate for {} is expired; offering anyway (the server clock is authoritative)",
                        username,
                    );
                }
                cert
            }
        };
        let res = handle
            .authenticate_openssh_cert(username, private_key.clone(), *cert)
            .await?;
        Ok(Some(res.into()))
    }

    /// Drive a keyboard-interactive exchange to completion.
    ///
    /// `_start` is called once, then we loop on `_respond` round by round
    /// until the server returns `Success` or `Failure` (a single auth can
    /// span several `InfoRequest` rounds, e.g. password then OTP). The loop
    /// is bounded so a misbehaving server can't pop prompts forever.
    ///
    /// Each round's answers come from one of three sources, in order:
    /// - `use_callback` + a `kbi_ask_tx` channel: surface the prompts to the
    ///   UI and wait for typed answers. The user cancelling (`None`) aborts
    ///   the auth cleanly (`Cancelled`).
    /// - otherwise `fallback_pw`: answer every prompt with the stored
    ///   password (the Auto path, and the headless degrade path).
    /// - neither available: fail cleanly (`Rejected`).
    ///
    /// A round carrying zero prompts is answered with an empty response, so
    /// servers that send an informational-only `InfoRequest` keep advancing.
    ///
    /// The `Rejected` / `Cancelled` split matters to the quick-connect Auto
    /// fallback: a server refusal may still fall through to a prompted
    /// password attempt, while an explicit user cancel must never chain a
    /// second modal. `Partial` means the exchange itself was ACCEPTED and
    /// the server requires one more of the carried methods (RFC 4252
    /// partial success, issue #125).
    pub(crate) async fn try_keyboard_interactive(
        &self,
        handle: &mut client::Handle<ClientHandler>,
        username: &str,
        fallback_pw: Option<&str>,
        use_callback: bool,
    ) -> Result<KbiOutcome, SshError> {
        // Cap on the number of challenge rounds. Real flows use 1-2; this is
        // just a backstop against a server that loops InfoRequest forever.
        const MAX_ROUNDS: usize = 16;

        // The outer auth-stage timeout is skipped for Interactive (it would
        // abort while the user types an OTP), so bound the individual network
        // round-trips here instead. The human-input wait below stays
        // unbounded but cancellable.
        let net_timeout = self.auth_timeout;
        let net_err = || {
            SshError::ConnectionFailed(format!(
                "keyboard-interactive server response timed out after {}s",
                net_timeout.as_secs()
            ))
        };

        let mut resp = tokio::time::timeout(
            net_timeout,
            handle.authenticate_keyboard_interactive_start(username, None::<String>),
        )
        .await
        .map_err(|_| net_err())??;

        // Guard for the TOTP autofill: only the FIRST OTP-looking round of
        // an attempt is answered automatically. A second one means the
        // server rejected the code (bad secret, clock drift), so the manual
        // modal takes over instead of feeding the same wrong code forever.
        let mut totp_used = false;

        for _ in 0..MAX_ROUNDS {
            let (name, instructions, prompts) = match resp {
                client::KeyboardInteractiveAuthResponse::Success => {
                    return Ok(KbiOutcome::Success);
                }
                client::KeyboardInteractiveAuthResponse::Failure {
                    remaining_methods,
                    partial_success,
                } => {
                    // Partial success: the exchange was ACCEPTED, the
                    // server wants one more method (issue #125).
                    return Ok(if partial_success {
                        KbiOutcome::Partial(remaining_methods)
                    } else {
                        KbiOutcome::Rejected
                    });
                }
                client::KeyboardInteractiveAuthResponse::InfoRequest {
                    name,
                    instructions,
                    prompts,
                } => (name, instructions, prompts),
            };

            let autofill = if totp_used {
                None
            } else {
                autofill_kbi_round(
                    self.totp.as_ref(),
                    round_context_wants_otp(&name, &instructions),
                    prompts.iter().map(|p| p.prompt.as_str()),
                    fallback_pw,
                )
            };

            let answers: Vec<String> = if prompts.is_empty() {
                Vec::new()
            } else if let Some(answers) = autofill {
                totp_used = true;
                answers
            } else if use_callback && let Some(tx) = self.kbi_ask_tx.as_ref() {
                let query = KbiQuery {
                    name,
                    instructions,
                    prompts: prompts
                        .iter()
                        .map(|p| KbiPromptField {
                            prompt: p.prompt.clone(),
                            echo: p.echo,
                        })
                        .collect(),
                };
                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                if tx.send((query, resp_tx)).await.is_err() {
                    // UI bridge is gone; treat as cancellation.
                    return Ok(KbiOutcome::Cancelled);
                }
                match resp_rx.await {
                    Ok(Some(answers)) => answers,
                    // User cancelled, or the responder dropped: abort cleanly.
                    Ok(None) | Err(_) => return Ok(KbiOutcome::Cancelled),
                }
            } else if let Some(pw) = fallback_pw {
                prompts.iter().map(|_| pw.to_string()).collect()
            } else {
                return Ok(KbiOutcome::Rejected);
            };

            resp = tokio::time::timeout(
                net_timeout,
                handle.authenticate_keyboard_interactive_respond(answers),
            )
            .await
            .map_err(|_| net_err())??;
        }

        tracing::warn!("keyboard-interactive exceeded {} rounds, giving up", MAX_ROUNDS);
        Ok(KbiOutcome::Rejected)
    }

    /// Authenticate via ssh-agent on Unix (Unix-domain sockets), trying
    /// EVERY discovered agent in order (issue #98): a live agent socket
    /// serving zero keys must not shadow another that holds the working
    /// key. The sweep itself (pinned-key pass across all agents,
    /// per-candidate dial bound, cross-agent offer dedup, per-agent
    /// tally on NoMatch) lives in `agent_auth_sweep`; this resolves the
    /// candidate sockets and the dial transport.
    #[cfg(unix)]
    pub(crate) async fn auth_via_agent(
        &self,
        handle: &mut client::Handle<ClientHandler>,
        username: &str,
    ) -> Result<AgentAuthOutcome, SshError> {
        let candidates =
            super::agent::unix_agent_sock_candidates(std::env::var("SSH_AUTH_SOCK").ok());
        if candidates.is_empty() {
            return Err(SshError::Key(
                "ssh-agent not available: SSH_AUTH_SOCK is not set".into(),
            ));
        }
        self.agent_auth_sweep(
            handle,
            username,
            &candidates,
            "ssh-agent not available",
            "no agent socket found",
            |path: &std::path::PathBuf| path.display().to_string(),
            |path: std::path::PathBuf| async move {
                russh::keys::agent::client::AgentClient::connect_uds(&path).await
            },
        )
        .await
    }

    /// Authenticate via Windows ssh-agents (named pipes). Same
    /// fallback-chain contract as the Unix variant above.
    #[cfg(windows)]
    pub(crate) async fn auth_via_agent(
        &self,
        handle: &mut client::Handle<ClientHandler>,
        username: &str,
    ) -> Result<AgentAuthOutcome, SshError> {
        let candidates = super::agent::agent_pipe_candidates();
        self.agent_auth_sweep(
            handle,
            username,
            &candidates,
            "Windows ssh-agent not available",
            "no agent pipe found",
            // The `\\.\pipe\` prefix is noise in a user-facing tally.
            |pipe: &String| pipe.trim_start_matches(r"\\.\pipe\").to_string(),
            |pipe: String| async move {
                russh::keys::agent::client::AgentClient::connect_named_pipe(&pipe).await
            },
        )
        .await
    }

    /// The shared multi-agent sweep behind both `auth_via_agent`
    /// variants. Two passes over the same candidate list:
    ///
    /// - Pass 1 (only when the connection pins a key, B3): dial each
    ///   agent and offer ONLY identities matching the pinned key. The
    ///   Oryxis endpoint (where vault keys live) is deliberately last
    ///   in the chain, so without this pass the earlier agents'
    ///   unrelated keys could burn the server's MaxAuthTries (sshd
    ///   default 6) before the pinned key was ever offered.
    /// - Pass 2: the full roster sweep, skipping whatever pass 1
    ///   already offered. It runs whenever pass 1 did not authenticate
    ///   (no pinned match anywhere, or the server rejected the pin),
    ///   preserving the documented try-all fallback after the pin.
    ///
    /// Each candidate's dial + LIST is individually bounded
    /// (`AGENT_DIAL_TIMEOUT`) so one wedged endpoint can't stall the
    /// sweep, and a dead server transport aborts the whole sweep with
    /// the real error instead of tallying up a misleading NoMatch
    /// (see `try_agent_identities`). The per-agent tally is recorded
    /// during pass 2 (which always runs when the sweep ends in
    /// NoMatch) and rides that outcome into the connection log, so an
    /// empty agent is visible instead of a bare "no keys matched".
    #[allow(clippy::too_many_arguments)]
    async fn agent_auth_sweep<D, T, Fut>(
        &self,
        handle: &mut client::Handle<ClientHandler>,
        username: &str,
        candidates: &[D],
        unavailable_prefix: &str,
        none_found: &str,
        display: impl Fn(&D) -> String,
        dial: impl Fn(D) -> Fut,
    ) -> Result<AgentAuthOutcome, SshError>
    where
        D: Clone,
        T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
        Fut: std::future::Future<
            Output = Result<russh::keys::agent::client::AgentClient<T>, russh::keys::Error>,
        >,
    {
        let mut offered: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut connected_any = false;

        // Pass 1: the pinned key only, across every agent. Dial and
        // list failures are not tallied here; pass 2 re-dials the same
        // endpoints and reports them.
        if let Some(pinned) = self.pinned_agent_key.clone() {
            for candidate in candidates {
                let DialStep::Listed(mut agent, identities) =
                    dial_and_list(&dial, candidate).await
                else {
                    continue;
                };
                connected_any = true;
                let matching: Vec<_> = identities
                    .into_iter()
                    .filter(|id| id.public_key().key_data() == pinned.key_data())
                    .filter(|id| !offered.contains(&identity_offer_tag(id)))
                    .collect();
                if matching.is_empty() {
                    continue;
                }
                match self
                    .try_agent_identities(handle, username, matching, &mut agent, &mut offered)
                    .await?
                {
                    AgentTry::Authenticated => return Ok(AgentAuthOutcome::Authenticated),
                    AgentTry::Partial(remaining) => {
                        return Ok(AgentAuthOutcome::Partial(remaining));
                    }
                    AgentTry::Exhausted => {}
                }
            }
        }

        // Pass 2: the full sweep. `offered` carries over, so anything
        // pass 1 already put in front of the server is never repeated.
        let mut report: Vec<String> = Vec::new();
        let mut last_err: Option<String> = None;
        for candidate in candidates {
            let disp = display(candidate);
            match dial_and_list(&dial, candidate).await {
                DialStep::Listed(mut agent, identities) => {
                    connected_any = true;
                    report.push(format!("{}: {} key(s)", disp, identities.len()));
                    let fresh: Vec<_> = identities
                        .into_iter()
                        .filter(|id| !offered.contains(&identity_offer_tag(id)))
                        .collect();
                    if fresh.is_empty() {
                        continue;
                    }
                    match self
                        .try_agent_identities(handle, username, fresh, &mut agent, &mut offered)
                        .await?
                    {
                        AgentTry::Authenticated => return Ok(AgentAuthOutcome::Authenticated),
                        AgentTry::Partial(remaining) => {
                            return Ok(AgentAuthOutcome::Partial(remaining));
                        }
                        AgentTry::Exhausted => {}
                    }
                }
                DialStep::ListError(e) => {
                    connected_any = true;
                    report.push(format!("{}: error {}", disp, e));
                }
                DialStep::TimedOut => {
                    report.push(format!("{}: timed out", disp));
                    last_err = Some(format!(
                        "{}: no response within {}s",
                        disp,
                        AGENT_DIAL_TIMEOUT.as_secs()
                    ));
                }
                DialStep::Unavailable(e) => {
                    report.push(format!("{}: unavailable", disp));
                    last_err = Some(format!("{}: {}", disp, e));
                }
            }
        }
        if !connected_any {
            return Err(SshError::Key(format!(
                "{}: {}",
                unavailable_prefix,
                last_err.unwrap_or_else(|| none_found.to_string()),
            )));
        }
        Ok(AgentAuthOutcome::NoMatch(report.join("; ")))
    }

    /// The shared per-agent auth loop: order the identities (the host's
    /// pinned key first, B3), then try each until one succeeds. NOTE:
    /// callers iterate several agents (see `agent_auth_sweep`); this
    /// runs one agent's roster.
    /// Certificate identities (an sk- cert loaded via `ssh-add`, or any
    /// agent-held cert) are offered as certificates; plain keys as
    /// publickey. The agent does the signing either way, so security-key
    /// signatures (authenticator flags + counter) pass through opaquely.
    ///
    /// An identity is marked in `offered` only after a round-trip the
    /// server actually saw (an `Ok` auth result, success or rejection),
    /// never at LIST time: an agent-side failure (confirm declined,
    /// agent died between LIST and sign) leaves the key unmarked so a
    /// later agent holding the same working key still gets to offer it.
    /// A dead server transport aborts the whole sweep with the real
    /// error instead of letting the tally misreport it as NoMatch.
    async fn try_agent_identities<S>(
        &self,
        handle: &mut client::Handle<ClientHandler>,
        username: &str,
        identities: Vec<russh::keys::agent::AgentIdentity>,
        agent: &mut S,
        offered: &mut std::collections::HashSet<String>,
    ) -> Result<AgentTry, SshError>
    where
        S: russh::Signer<Error = russh::AgentAuthError>,
    {
        // Server-advertised RSA hash is per-connection, resolved once
        // (not per key) so a multi-key agent doesn't burn MaxAuthTries.
        let rsa_hash = server_rsa_hash(handle).await;
        for identity in
            select_agent_identities(identities, self.pinned_agent_key.as_ref())
        {
            let tag = identity_offer_tag(&identity);
            let pubkey = identity.public_key().into_owned();
            let fingerprint = pubkey.fingerprint(russh::keys::HashAlg::Sha256);
            let hash = if pubkey.algorithm().is_rsa() { rsa_hash } else { None };
            let res = match identity {
                russh::keys::agent::AgentIdentity::Certificate { certificate, .. } => {
                    handle
                        .authenticate_certificate_with(username, certificate, hash, agent)
                        .await
                }
                russh::keys::agent::AgentIdentity::PublicKey { .. } => {
                    handle
                        .authenticate_publickey_with(username, pubkey, hash, agent)
                        .await
                }
            };
            match res {
                Ok(res) => {
                    // The server saw this offer: never repeat it from a
                    // later agent (that burns MaxAuthTries).
                    offered.insert(tag);
                    match res.into() {
                        StepVerdict::Accepted => return Ok(AgentTry::Authenticated),
                        // Partial success: this key WAS accepted, the
                        // server wants another factor. Offering further
                        // keys would answer a question the server is no
                        // longer asking; hand the continuation up.
                        StepVerdict::Partial(remaining) => {
                            return Ok(AgentTry::Partial(remaining));
                        }
                        StepVerdict::Rejected => {}
                    }
                    // A "failure" on a gone transport is a disconnect
                    // (MaxAuthTries exhausted, server gave up), not a
                    // rejection: russh reports a closed reply channel
                    // as a plain failure, so check the handle itself.
                    if handle.is_closed() {
                        return Err(SshError::ConnectionFailed(
                            "server closed the connection during agent auth \
                             (too many authentication attempts?)"
                                .into(),
                        ));
                    }
                }
                // The transport to the server died mid-attempt: no
                // later key or agent can salvage this connection, stop.
                Err(russh::AgentAuthError::Send(e)) => {
                    return Err(SshError::ConnectionFailed(format!(
                        "server connection lost during agent auth: {}",
                        e
                    )));
                }
                // Agent-side failure (signature refused, confirm
                // declined, agent died). The server never received a
                // completed offer, so the key stays unmarked and a
                // later agent holding it can still authenticate.
                Err(russh::AgentAuthError::Key(e)) => {
                    tracing::warn!("agent failed to sign with {}: {}", fingerprint, e);
                }
            }
        }
        Ok(AgentTry::Exhausted)
    }

    /// Authenticate and open a PTY session on the handle.
    pub(crate) async fn authenticate_and_open(
        &self,
        mut handle: client::Handle<ClientHandler>,
        connection: &Connection,
        password: Option<&str>,
        key_material: Option<KeyMaterial<'_>>,
        cols: u32,
        rows: u32,
    ) -> Result<(SshSession, mpsc::UnboundedReceiver<Vec<u8>>), SshError> {
        // Apply the same per-phase timeouts the public 2-step API uses
        //, single-call connects via `connect_with_resolver` were
        // bypassing them, leaving auth/session free to hang on the OS
        // default ceilings. Auth honours the Interactive exemption (human
        // input isn't a network stall) via `authenticate_handle_bounded`.
        let session_timeout = self.session_timeout;
        self.authenticate_handle_bounded(&mut handle, connection, password, key_material)
            .await?;
        let listeners = bind_port_forward_listeners(&connection.port_forwards).await?;
        let (mut session, rx) = tokio::time::timeout(
            session_timeout,
            self.open_pty_session(super::SshTransport::new(handle), cols, rows, listeners),
        )
        .await
        .map_err(|_| {
            SshError::ConnectionFailed(format!(
                "session open timed out after {}s",
                session_timeout.as_secs()
            ))
        })??;
        session.sftp_open_timeout = session_timeout;
        Ok((session, rx))
    }
}

/// The server's verdict on one completed auth offer, folding russh's
/// `AuthResult` and RFC 4252 partial success into a three-way triage.
/// `Partial` means the offered factor was ACCEPTED but the server
/// requires more before granting access, carrying the methods it will
/// take next; treating it as a plain rejection is exactly the bug that
/// broke 2FA servers (issue #125).
pub(crate) enum StepVerdict {
    Accepted,
    Rejected,
    Partial(MethodSet),
}

impl From<client::AuthResult> for StepVerdict {
    fn from(res: client::AuthResult) -> Self {
        match res {
            client::AuthResult::Success => StepVerdict::Accepted,
            client::AuthResult::Failure { remaining_methods, partial_success } => {
                if partial_success {
                    StepVerdict::Partial(remaining_methods)
                } else {
                    StepVerdict::Rejected
                }
            }
        }
    }
}

/// Result of the multi-agent auth sweep in `auth_via_agent`. `NoMatch`
/// carries the per-agent key tally (endpoint: N key(s), ...) so the
/// surfaced error explains WHICH agent had nothing instead of a bare
/// "no keys matched" (issue #98). `Partial` is RFC 4252 partial success:
/// an agent key was accepted, the server requires more (issue #125).
pub(crate) enum AgentAuthOutcome {
    Authenticated,
    NoMatch(String),
    Partial(MethodSet),
}

/// One agent's roster attempt inside the sweep (`try_agent_identities`).
/// `Partial` stops the sweep: a key was accepted as the first factor, so
/// offering more keys would answer a question the server stopped asking.
enum AgentTry {
    Authenticated,
    Partial(MethodSet),
    /// Every fresh identity was offered and rejected (or skipped).
    Exhausted,
}

/// Per-candidate bound on dialing one agent endpoint and listing its
/// identities. A local socket / named-pipe round-trip is normally
/// sub-millisecond, so 3 seconds is pure headroom for a loaded machine
/// while staying small enough that one wedged endpoint (russh's
/// named-pipe connect retries ERROR_PIPE_BUSY in an unbounded 50ms
/// loop) can't eat the 30s auth budget or hang quick-connect, whose
/// interactive path has no blanket auth timeout at all: the worst case
/// of 4 Windows candidates across both sweep passes is 8 dials = 24s.
/// Signing is deliberately NOT bounded: a confirm-gated agent (the
/// Oryxis confirm modal, a KeePassXC dialog, a FIDO2 touch) waits on
/// the user, legitimately and indefinitely.
const AGENT_DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// One candidate's dial + LIST outcome inside the agent sweep.
enum DialStep<T: AsyncRead + AsyncWrite> {
    /// Connected and listed; the client stays open for signing.
    Listed(
        russh::keys::agent::client::AgentClient<T>,
        Vec<russh::keys::agent::AgentIdentity>,
    ),
    /// Connected, but the LIST request failed.
    ListError(String),
    /// The endpoint gave no answer within `AGENT_DIAL_TIMEOUT`.
    TimedOut,
    /// Could not connect at all.
    Unavailable(String),
}

/// Dial one agent endpoint and list its identities, bounded by
/// `AGENT_DIAL_TIMEOUT` so a wedged pipe can't stall the sweep: the
/// next candidate always gets its turn.
async fn dial_and_list<D, T, Fut>(dial: &impl Fn(D) -> Fut, candidate: &D) -> DialStep<T>
where
    D: Clone,
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    Fut: std::future::Future<
        Output = Result<russh::keys::agent::client::AgentClient<T>, russh::keys::Error>,
    >,
{
    tokio::time::timeout(AGENT_DIAL_TIMEOUT, async {
        match dial(candidate.clone()).await {
            Ok(mut agent) => match agent.request_identities().await {
                Ok(identities) => DialStep::Listed(agent, identities),
                Err(e) => DialStep::ListError(e.to_string()),
            },
            Err(e) => DialStep::Unavailable(e.to_string()),
        }
    })
    .await
    .unwrap_or(DialStep::TimedOut)
}

/// The dedup tag for one agent identity in the cross-agent sweep: a tag
/// already in `offered` is skipped by later agents. Certificates and
/// plain keys with the same underlying key are DIFFERENT offers (cert
/// auth vs publickey auth), hence the kind prefix. A certificate tag
/// covers the FULL cert blob, not the underlying key's fingerprint: a
/// stale cert in one agent and a freshly reissued cert for the same key
/// in another are distinct credentials the server judges on their own
/// validity, so only byte-identical certs may dedup. The comment-free
/// wire encoding is base64'd rather than fingerprinted so equal tags
/// always mean equal certs; on the never-expected encode failure the
/// fallback still separates reissues by serial + validity window.
fn identity_offer_tag(identity: &russh::keys::agent::AgentIdentity) -> String {
    match identity {
        russh::keys::agent::AgentIdentity::Certificate { certificate, .. } => {
            use base64::Engine as _;
            match certificate.to_bytes() {
                Ok(blob) => format!(
                    "cert:{}",
                    base64::engine::general_purpose::STANDARD.encode(blob)
                ),
                Err(_) => format!(
                    "cert:{}:{}:{}",
                    identity
                        .public_key()
                        .fingerprint(russh::keys::HashAlg::Sha256),
                    certificate.serial(),
                    certificate.valid_before(),
                ),
            }
        }
        russh::keys::agent::AgentIdentity::PublicKey { key, .. } => {
            format!("key:{}", key.fingerprint(russh::keys::HashAlg::Sha256))
        }
    }
}

/// Order agent identities so the pinned key (the host's referenced vault
/// key, B3) is offered FIRST, preserving the try-all fallback after it.
/// Comparison is on key data, so a certificate identity whose underlying
/// key matches the pin also sorts first. A pin matching nothing (dangling
/// `key_id`, key not loaded in the agent) leaves the order untouched.
/// Pure, so it unit-tests without an agent socket.
fn select_agent_identities(
    identities: Vec<russh::keys::agent::AgentIdentity>,
    pinned: Option<&russh::keys::PublicKey>,
) -> Vec<russh::keys::agent::AgentIdentity> {
    let Some(pinned) = pinned else {
        return identities;
    };
    let (mut matching, rest): (Vec<_>, Vec<_>) = identities
        .into_iter()
        .partition(|id| id.public_key().key_data() == pinned.key_data());
    matching.extend(rest);
    matching
}

/// The result of validating an attached certificate against its private
/// key, before any network round-trip. Pure so it is unit-testable
/// without a live server (the `authenticate_openssh_cert` call is not).
enum CertCheck {
    /// Parsed and certifies this key; offer it. `expired` drives an
    /// advisory warning only (the server's clock is authoritative). The
    /// certificate is boxed (it dwarfs the `Unusable` variant).
    Offer {
        cert: Box<russh::keys::Certificate>,
        expired: bool,
    },
    /// Unusable (unparseable, or it does not certify this key): the
    /// caller should fall back to the bare public key.
    Unusable(&'static str),
}

/// Validate `cert_line` against `private_key` at wall-clock `now_unix`
/// (0 = unknown, skips the expiry check). Never fails: a bad cert is a
/// `Unusable`, so the auth path can always degrade to the plain key.
fn check_certificate(
    cert_line: &str,
    private_key: &russh::keys::PrivateKey,
    now_unix: u64,
) -> CertCheck {
    let cert = match russh::keys::Certificate::from_openssh(cert_line) {
        Ok(c) => c,
        Err(_) => return CertCheck::Unusable("unparseable"),
    };
    // The certificate must certify exactly this private key.
    if cert.public_key() != private_key.public_key().key_data() {
        return CertCheck::Unusable("does not match the private key");
    }
    let expired = now_unix != 0 && cert.valid_before() != 0 && now_unix > cert.valid_before();
    CertCheck::Offer { cert: Box::new(cert), expired }
}

#[cfg(test)]
mod cert_tests {
    use super::{check_certificate, CertCheck};
    use russh::keys::ssh_key::{certificate, Algorithm, PrivateKey};

    /// A CA-signed user certificate for `user_key`, valid across `now`,
    /// as its OpenSSH public line.
    fn make_cert(user_key: &PrivateKey, valid_before: u64) -> String {
        let ca = PrivateKey::random(&mut rand010::rng(), Algorithm::Ed25519).unwrap();
        let mut builder = certificate::Builder::new_with_random_nonce(
            &mut rand010::rng(),
            user_key.public_key(),
            0, // valid_after: the beginning of time
            valid_before,
        )
        .unwrap();
        builder.serial(1).unwrap();
        builder.key_id("t").unwrap();
        builder.cert_type(certificate::CertType::User).unwrap();
        builder.valid_principal("tester").unwrap();
        builder.sign(&ca).unwrap().to_openssh().unwrap()
    }

    #[test]
    fn matching_cert_is_offered() {
        let key = PrivateKey::random(&mut rand010::rng(), Algorithm::Ed25519).unwrap();
        let cert = make_cert(&key, 4_000_000_000); // far future
        match check_certificate(&cert, &key, 1_700_000_000) {
            CertCheck::Offer { expired, .. } => assert!(!expired),
            CertCheck::Unusable(w) => panic!("expected offer, got {w}"),
        }
    }

    #[test]
    fn expired_cert_is_still_offered_flagged() {
        let key = PrivateKey::random(&mut rand010::rng(), Algorithm::Ed25519).unwrap();
        let cert = make_cert(&key, 1_000); // long past
        match check_certificate(&cert, &key, 1_700_000_000) {
            CertCheck::Offer { expired, .. } => assert!(expired, "should flag expiry"),
            CertCheck::Unusable(w) => panic!("expired cert must still be offered, got {w}"),
        }
    }

    #[test]
    fn cert_for_another_key_is_unusable() {
        let key = PrivateKey::random(&mut rand010::rng(), Algorithm::Ed25519).unwrap();
        let other = PrivateKey::random(&mut rand010::rng(), Algorithm::Ed25519).unwrap();
        let cert = make_cert(&other, 4_000_000_000); // certifies `other`, not `key`
        assert!(matches!(
            check_certificate(&cert, &key, 1_700_000_000),
            CertCheck::Unusable(_)
        ));
    }

    #[test]
    fn garbage_cert_line_is_unusable() {
        let key = PrivateKey::random(&mut rand010::rng(), Algorithm::Ed25519).unwrap();
        assert!(matches!(
            check_certificate("not a certificate", &key, 0),
            CertCheck::Unusable(_)
        ));
    }
}

#[cfg(test)]
mod agent_dedup_tests {
    use super::identity_offer_tag;
    use russh::keys::agent::AgentIdentity;
    use russh::keys::ssh_key::{certificate, Algorithm, Certificate, PrivateKey};

    /// A CA-signed user certificate for `user_key`. The random nonce
    /// makes every call produce a distinct blob, like a real reissue.
    fn make_cert(user_key: &PrivateKey, serial: u64, valid_before: u64) -> Certificate {
        let ca = PrivateKey::random(&mut rand010::rng(), Algorithm::Ed25519).unwrap();
        let mut builder = certificate::Builder::new_with_random_nonce(
            &mut rand010::rng(),
            user_key.public_key(),
            0, // valid_after: the beginning of time
            valid_before,
        )
        .unwrap();
        builder.serial(serial).unwrap();
        builder.key_id("t").unwrap();
        builder.cert_type(certificate::CertType::User).unwrap();
        builder.valid_principal("tester").unwrap();
        builder.sign(&ca).unwrap()
    }

    fn cert_identity(certificate: Certificate) -> AgentIdentity {
        AgentIdentity::Certificate { certificate, comment: String::new() }
    }

    #[test]
    fn cert_and_bare_key_are_different_offers() {
        // Cert auth and publickey auth are separate offers even for
        // the same underlying key, hence the kind prefix in the tag.
        let key = PrivateKey::random(&mut rand010::rng(), Algorithm::Ed25519).unwrap();
        let bare = AgentIdentity::PublicKey {
            key: key.public_key().clone(),
            comment: String::new(),
        };
        let cert = cert_identity(make_cert(&key, 1, 4_000_000_000));
        assert_ne!(identity_offer_tag(&bare), identity_offer_tag(&cert));
    }

    #[test]
    fn distinct_certs_for_same_key_get_distinct_tags() {
        // A stale cert in agent A must not shadow a freshly reissued
        // cert for the SAME underlying key in agent B: the tag covers
        // the full cert blob, not the underlying key's fingerprint.
        let key = PrivateKey::random(&mut rand010::rng(), Algorithm::Ed25519).unwrap();
        let stale = cert_identity(make_cert(&key, 1, 1_000));
        let fresh = cert_identity(make_cert(&key, 2, 4_000_000_000));
        assert_ne!(identity_offer_tag(&stale), identity_offer_tag(&fresh));
    }

    #[test]
    fn identical_cert_across_agents_shares_one_tag() {
        // The same cert loaded into two agents is one credential: it
        // must dedup, whatever comment each agent attached to it.
        let key = PrivateKey::random(&mut rand010::rng(), Algorithm::Ed25519).unwrap();
        let cert = make_cert(&key, 1, 4_000_000_000);
        let mut relabeled = cert.clone();
        relabeled.set_comment("same cert, different agent");
        let a = AgentIdentity::Certificate {
            certificate: cert,
            comment: "agent-a".into(),
        };
        let b = AgentIdentity::Certificate {
            certificate: relabeled,
            comment: "agent-b".into(),
        };
        assert_eq!(identity_offer_tag(&a), identity_offer_tag(&b));
    }

    #[test]
    fn distinct_plain_keys_get_distinct_tags() {
        let a = PrivateKey::random(&mut rand010::rng(), Algorithm::Ed25519).unwrap();
        let b = PrivateKey::random(&mut rand010::rng(), Algorithm::Ed25519).unwrap();
        let ida = AgentIdentity::PublicKey {
            key: a.public_key().clone(),
            comment: String::new(),
        };
        let idb = AgentIdentity::PublicKey {
            key: b.public_key().clone(),
            comment: String::new(),
        };
        assert_ne!(identity_offer_tag(&ida), identity_offer_tag(&idb));
    }
}

#[cfg(test)]
mod agent_pin_tests {
    use super::select_agent_identities;
    use russh::keys::agent::AgentIdentity;
    use russh::keys::PublicKey;

    // Public security-key fixture from the ssh-key crate's test suite
    // (public material only, nothing secret).
    const SK_ED25519_PUB: &str = "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAICFo/k5LU8863u66YC9eUO2170QduohPURkQnbLa/dczAAAABHNzaDo= user@example.com";

    fn plain(seed: u8) -> AgentIdentity {
        // Deterministic distinct Ed25519 keys derived from a seed byte.
        use russh::keys::ssh_key;
        let secret = ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]);
        AgentIdentity::PublicKey {
            key: PublicKey::new(ssh_key::public::KeyData::Ed25519(secret.public), ""),
            comment: format!("key-{seed}"),
        }
    }

    fn sk_identity() -> AgentIdentity {
        AgentIdentity::from(PublicKey::from_openssh(SK_ED25519_PUB).unwrap())
    }

    fn labels(ids: &[AgentIdentity]) -> Vec<String> {
        ids.iter().map(|i| i.comment().to_string()).collect()
    }

    #[test]
    fn no_pin_keeps_order() {
        let ids = vec![plain(1), plain(2), sk_identity()];
        let expect = labels(&ids);
        let ordered = select_agent_identities(ids, None);
        assert_eq!(labels(&ordered), expect);
    }

    #[test]
    fn pinned_identity_moves_first_and_rest_follow() {
        let pinned = PublicKey::from_openssh(SK_ED25519_PUB).unwrap();
        let ids = vec![plain(1), plain(2), sk_identity(), plain(3)];
        let ordered = select_agent_identities(ids, Some(&pinned));
        assert_eq!(ordered.len(), 4);
        assert_eq!(
            ordered[0].public_key().key_data(),
            pinned.key_data(),
            "pinned identity must be offered first"
        );
        // Try-all fallback preserved in original relative order.
        assert_eq!(labels(&ordered)[1..], ["key-1", "key-2", "key-3"]);
    }

    #[test]
    fn dangling_pin_leaves_order_untouched() {
        let pinned = PublicKey::from_openssh(SK_ED25519_PUB).unwrap();
        let ids = vec![plain(1), plain(2)];
        let expect = labels(&ids);
        let ordered = select_agent_identities(ids, Some(&pinned));
        assert_eq!(labels(&ordered), expect);
    }
}
