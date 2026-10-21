//! 香港信用卡资料仅存于 CheckoutRunner 内存，不参与设置序列化或日志。
use serde::Deserialize;
use serde_json::json;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HkPayment {
    card_number: String,
    expiration: String,
    security_code: String,
    billing_name: String,
    street: String,
    #[serde(default)]
    street2: String,
}

impl HkPayment {
    pub fn validate(mut self) -> Result<Self, String> {
        self.card_number = self.card_number.replace([' ', '-'], "");
        self.expiration = self.expiration.replace(' ', "");
        self.security_code = self.security_code.trim().into();
        self.billing_name = self.billing_name.trim().into();
        self.street = self.street.trim().into();
        self.street2 = self.street2.trim().into();
        let expiry = self.expiration.as_bytes();
        let month = self.expiration.get(..2).and_then(|s| s.parse::<u8>().ok());
        if !(12..=19).contains(&self.card_number.len())
            || !self.card_number.bytes().all(|b| b.is_ascii_digit())
            || expiry.len() != 5
            || expiry[2] != b'/'
            || !month.is_some_and(|m| (1..=12).contains(&m))
            || !expiry[3..].iter().all(u8::is_ascii_digit)
            || !(3..=4).contains(&self.security_code.len())
            || !self.security_code.bytes().all(|b| b.is_ascii_digit())
            || self.billing_name.is_empty()
            || self.street.is_empty()
        {
            return Err(
                "请填写有效的香港信用卡资料、MM/YY 有效期、3–4 位安全码和账单姓名及地址".into(),
            );
        }
        Ok(self)
    }

    pub fn script(&self) -> String {
        format!(
            "JSON.stringify(({})({}))",
            include_str!("checkout_hk_billing.js"),
            json!({
                "cardNumber": self.card_number,
                "expiration": self.expiration,
                "securityCode": self.security_code,
                "billingName": self.billing_name,
                "street": self.street,
                "street2": self.street2,
            })
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 信用卡资料校验失败不回显输入() {
        let payment: HkPayment = serde_json::from_value(json!({
            "cardNumber": "synthetic-secret", "expiration": "13/30", "securityCode": "123",
            "billingName": "Test", "street": "Test Street",
        }))
        .unwrap();
        let error = payment.validate().err().unwrap();
        assert!(!error.contains("synthetic-secret"));
    }

    #[test]
    fn 信用卡资料规范化后只生成填表脚本() {
        let payment: HkPayment = serde_json::from_value(json!({
            "cardNumber": "4111-1111 1111 1111", "expiration": "12 / 30", "securityCode": "123",
            "billingName": " Test ", "street": " Test Street ",
        }))
        .unwrap();
        let payment = payment.validate().unwrap();
        assert_eq!(payment.card_number, "4111111111111111");
        assert_eq!(payment.expiration, "12/30");
        assert_eq!(payment.billing_name, "Test");
        assert!(payment.script().contains("Test Street"));
    }
}
