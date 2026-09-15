//! Read Electron's macOS safeStorage v10 format through CommonCrypto.
//! The parameters match Chromium's os_crypt_mac.mm (PBKDF2-SHA1, AES-128-CBC).
//! Format reference:
//! https://raw.githubusercontent.com/chromium/chromium/130.0.6723.58/components/os_crypt/sync/os_crypt_mac.mm
//! Keychain data and decrypted values stay in memory; this module never writes.

use std::ffi::{c_char, c_void};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[link(name = "System")]
extern "C" {
    fn CCKeyDerivationPBKDF(
        algorithm: u32,
        password: *const c_char,
        password_len: usize,
        salt: *const u8,
        salt_len: usize,
        prf: u32,
        rounds: u32,
        derived_key: *mut u8,
        derived_key_len: usize,
    ) -> i32;
    fn CCCrypt(
        operation: u32,
        algorithm: u32,
        options: u32,
        key: *const c_void,
        key_len: usize,
        iv: *const c_void,
        input: *const c_void,
        input_len: usize,
        output: *mut c_void,
        output_capacity: usize,
        output_len: *mut usize,
    ) -> i32;
}

// Deliberately no Debug implementation: even a derived key is a secret.
pub(crate) struct Key([u8; 16]);

impl Key {
    pub(crate) fn from_keychain(service: &str) -> Result<Self, String> {
        let denied = || {
            "Cannot access the Grok Bot login in Keychain. Allow access to Grok Bot Safe Storage, then refresh.".to_string()
        };
        let mut child = Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", service, "-w"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| denied())?;
        // An unanswered OS prompt must not hang the combined quota poll.
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if started.elapsed() < Duration::from_secs(25) => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(denied());
                }
            }
        }
        let mut output = child.wait_with_output().map_err(|_| denied())?;
        if !output.status.success() {
            return Err(denied());
        }
        while matches!(output.stdout.last(), Some(b'\r' | b'\n')) {
            output.stdout.pop();
        }
        if output.stdout.is_empty() {
            return Err(denied());
        }
        let key = Self::from_password(&output.stdout);
        erase(&mut output.stdout);
        key
    }

    fn from_password(password: &[u8]) -> Result<Self, String> {
        let mut key = Self([0; 16]);
        let salt = b"saltysalt";
        // SAFETY: all pointers refer to live slices with their exact lengths;
        // CommonCrypto writes at most the supplied 16-byte key buffer.
        let status = unsafe {
            CCKeyDerivationPBKDF(
                2,
                password.as_ptr().cast(),
                password.len(),
                salt.as_ptr(),
                salt.len(),
                1,
                1003,
                key.0.as_mut_ptr(),
                key.0.len(),
            )
        };
        if status != 0 {
            return Err("Could not derive the Grok Bot login key.".to_string());
        }
        Ok(key)
    }

    pub(crate) fn decrypt(&self, ciphertext: &[u8]) -> Result<String, String> {
        let invalid = || {
            "Cannot decrypt the Grok Bot login. Open Grok Bot and sign in again, then refresh."
                .to_string()
        };
        let data = ciphertext.strip_prefix(b"v10").ok_or_else(invalid)?;
        if data.is_empty() || data.len() % 16 != 0 {
            return Err(invalid());
        }
        let mut output = vec![0; data.len()];
        let mut written = 0;
        let iv = [b' '; 16];
        // SAFETY: the key and IV are 16 bytes; input and output are distinct
        // live buffers. CBC decryption with PKCS7 never grows the input.
        let status = unsafe {
            CCCrypt(
                1,
                0,
                1,
                self.0.as_ptr().cast(),
                self.0.len(),
                iv.as_ptr().cast(),
                data.as_ptr().cast(),
                data.len(),
                output.as_mut_ptr().cast(),
                output.len(),
                &mut written,
            )
        };
        if status != 0 {
            erase(&mut output);
            return Err(invalid());
        }
        output.truncate(written);
        String::from_utf8(output).map_err(|error| {
            // Do not include decrypted bytes in an error message.
            let mut bytes = error.into_bytes();
            erase(&mut bytes);
            invalid()
        })
    }
}

fn erase(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: each pointer is an exclusively borrowed live byte. Volatile
        // stores prevent this secret cleanup from being optimized away.
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        erase(&mut self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn decrypts_electron_v10_fixture_and_rejects_invalid_data() {
        // Synthetic password and ciphertext generated independently with
        // Python hashlib + OpenSSL, never from a user's Keychain.
        let encrypted = base64::engine::general_purpose::STANDARD
            .decode("djEwAO+8/FAQYBswbLPYv43X0Q==")
            .unwrap();
        let key = Key::from_password(b"test-keychain-password").unwrap();
        assert_eq!(key.decrypt(&encrypted).unwrap(), "test-token-1234");
        assert!(Key::from_password(b"wrong-password")
            .unwrap()
            .decrypt(&encrypted)
            .is_err());
        assert!(key.decrypt(b"v11unsupported").is_err());
        assert!(key.decrypt(b"v10truncated").is_err());
    }
}
