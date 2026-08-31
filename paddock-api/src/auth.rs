//! 认证与凭据工具：Argon2id 密码哈希、token 签发/校验（90 天滑动）。
//! 用户名规则（定案）：1–16 字符，中文/字母/数字 + 相邻单空格（两侧禁），注册后不可改。

use argon2::Argon2;
use argon2::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};
use rand::Rng;

pub const TOKEN_TTL_DAYS: i64 = 90;
pub const REG_CODE_TTL_MINUTES: i64 = 30;

/// 校验用户名：1–16 字符；允许中文（\\u{4E00}-\\u{9FFF}）、字母、数字；
/// 空格只允许出现在字符之间且不连续（"张三"、"Zhang San" 合法；" 张三"、"张 三 "、"张  三" 非法）。
pub fn validate_username(name: &str) -> Result<(), &'static str> {
    let trimmed = name.trim_matches(' ');
    if trimmed != name {
        return Err("用户名不能以空格开头或结尾");
    }
    if trimmed.is_empty() {
        return Err("用户名不能为空");
    }
    let mut prev_space = false;
    for (i, ch) in trimmed.chars().enumerate() {
        let ok = ch.is_ascii_alphanumeric() || ('\u{4E00}'..='\u{9FFF}').contains(&ch);
        if ch == ' ' {
            if prev_space || i == 0 {
                return Err("空格只允许出现在字符之间且不连续");
            }
            prev_space = true;
            continue;
        }
        if !ok {
            return Err("用户名只允许中文、字母、数字和字符间单个空格");
        }
        prev_space = false;
    }
    if trimmed.chars().count() > 16 {
        return Err("用户名最长 16 个字符");
    }
    Ok(())
}

/// 校验密码：≥8 位且同时含数字与字母。
pub fn validate_password(pw: &str) -> Result<(), &'static str> {
    let has_digit = pw.chars().any(|c| c.is_ascii_digit());
    let has_alpha = pw.chars().any(|c| c.is_ascii_alphabetic());
    if pw.len() < 8 || !has_digit || !has_alpha {
        return Err("密码至少 8 位，且须同时包含数字和字母");
    }
    Ok(())
}

pub fn hash_password(pw: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hash: {e}"))?
        .to_string())
}

pub fn verify_password(pw: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .and_then(|parsed| Argon2::default().verify_password(pw.as_bytes(), &parsed))
        .is_ok()
}

/// 签发新 token + 返回其 SHA-256 哈希（库内只存哈希）。
pub fn issue_token() -> anyhow::Result<(String, String)> {
    let raw: String = rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();
    let hash = sha256_hex(&raw);
    Ok((raw, hash))
}

pub fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    hex::encode(h.finalize())
}

/// 注册校验码："申请围场通行证#" 后的一串随机字母数字（8 位，去易混淆字符）。
pub fn gen_reg_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789"; // 去 I/L/O/0/1
    let mut rng = rand::rng();
    (0..8)
        .map(|_| {
            let i = rng.random_range(0..ALPHABET.len());
            ALPHABET[i] as char
        })
        .collect()
}
