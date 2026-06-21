use core::fmt;
use core::str::FromStr;

use crate::Error;

/// 48-bit Bluetooth device address.
///
/// Parsed from the canonical `aa:bb:cc:dd:ee:ff` form. The internal byte
/// order is **little-endian** (i.e. the byte that prints first sits at
/// index 5), matching Linux `sockaddr_rc` and Windows `BTH_ADDR` storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BdAddr(pub(crate) [u8; 6]);

impl BdAddr {
    #[must_use]
    pub const fn raw(self) -> [u8; 6] {
        self.0
    }

    #[must_use]
    pub const fn as_u64(self) -> u64 {
        u64::from_le_bytes([
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5], 0, 0,
        ])
    }
}

impl FromStr for BdAddr {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut bytes = [0u8; 6];
        let mut parts = s.split(':');
        for slot in bytes.iter_mut().rev() {
            let part = parts
                .next()
                .ok_or_else(|| Error::InvalidAddress(s.to_owned()))?;
            if part.len() != 2 || !part.as_bytes().iter().all(u8::is_ascii_hexdigit) {
                return Err(Error::InvalidAddress(s.to_owned()));
            }
            *slot =
                u8::from_str_radix(part, 16).map_err(|_| Error::InvalidAddress(s.to_owned()))?;
        }
        if parts.next().is_some() {
            return Err(Error::InvalidAddress(s.to_owned()));
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for BdAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[5], b[4], b[3], b[2], b[1], b[0]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_address() {
        let addr: BdAddr = "80:C3:BA:2A:6A:1D".parse().unwrap();
        assert_eq!(addr.raw(), [0x1D, 0x6A, 0x2A, 0xBA, 0xC3, 0x80]);
        assert_eq!(addr.as_u64(), 0x0000_80C3_BA2A_6A1D);
        assert_eq!(addr.to_string(), "80:c3:ba:2a:6a:1d");
    }

    #[test]
    fn rejects_wrong_length() {
        assert!("80:C3:BA:2A:6A".parse::<BdAddr>().is_err());
        assert!("80:C3:BA:2A:6A:1D:FF".parse::<BdAddr>().is_err());
    }

    #[test]
    fn rejects_non_hex() {
        assert!("80:C3:BA:2A:6A:ZZ".parse::<BdAddr>().is_err());
    }

    #[test]
    fn rejects_non_canonical_groups() {
        assert!("1:2:3:4:5:6".parse::<BdAddr>().is_err());
        assert!("080:C3:BA:2A:6A:1D".parse::<BdAddr>().is_err());
    }
}
