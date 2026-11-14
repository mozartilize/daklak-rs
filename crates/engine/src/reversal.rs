//! Composed-glyph reversal tables shared by Telex, VNI, and VIQR.

use super::InputMethod;

pub(super) const GLYPHS: &[(char, &str, &str, &str)] = &[
    ('À', "Af", "A2", "A`"),
    ('Á', "As", "A1", "A'"),
    ('Â', "Aa", "A6", "A^"),
    ('Ã', "Ax", "A4", "A~"),
    ('È', "Ef", "E2", "E`"),
    ('É', "Es", "E1", "E'"),
    ('Ê', "Ee", "E6", "E^"),
    ('Ì', "If", "I2", "I`"),
    ('Í', "Is", "I1", "I'"),
    ('Ò', "Of", "O2", "O`"),
    ('Ó', "Os", "O1", "O'"),
    ('Ô', "Oo", "O6", "O^"),
    ('Õ', "Ox", "O4", "O~"),
    ('Ù', "Uf", "U2", "U`"),
    ('Ú', "Us", "U1", "U'"),
    ('Ý', "Ys", "Y1", "Y'"),
    ('à', "af", "a2", "a`"),
    ('á', "as", "a1", "a'"),
    ('â', "aa", "a6", "a^"),
    ('ã', "ax", "a4", "a~"),
    ('è', "ef", "e2", "e`"),
    ('é', "es", "e1", "e'"),
    ('ê', "ee", "e6", "e^"),
    ('ì', "if", "i2", "i`"),
    ('í', "is", "i1", "i'"),
    ('ò', "of", "o2", "o`"),
    ('ó', "os", "o1", "o'"),
    ('ô', "oo", "o6", "o^"),
    ('õ', "ox", "o4", "o~"),
    ('ù', "uf", "u2", "u`"),
    ('ú', "us", "u1", "u'"),
    ('ý', "ys", "y1", "y'"),
    ('Ă', "Aw", "A8", "A("),
    ('ă', "aw", "a8", "a("),
    ('Đ', "Dd", "D9", "DD"),
    ('đ', "dd", "d9", "dd"),
    ('Ĩ', "Ix", "I4", "I~"),
    ('ĩ', "ix", "i4", "i~"),
    ('Ũ', "Ux", "U4", "U~"),
    ('ũ', "ux", "u4", "u~"),
    ('Ơ', "Ow", "O7", "O+"),
    ('ơ', "ow", "o7", "o+"),
    ('Ư', "Uw", "U7", "U+"),
    ('ư', "uw", "u7", "u+"),
    ('Ạ', "Aj", "A5", "A."),
    ('ạ', "aj", "a5", "a."),
    ('Ả', "Ar", "A3", "A?"),
    ('ả', "ar", "a3", "a?"),
    ('Ấ', "Aas", "A61", "A^'"),
    ('ấ', "aas", "a61", "a^'"),
    ('Ầ', "Aaf", "A62", "A^`"),
    ('ầ', "aaf", "a62", "a^`"),
    ('Ẩ', "Aar", "A63", "A^?"),
    ('ẩ', "aar", "a63", "a^?"),
    ('Ẫ', "Aax", "A64", "A^~"),
    ('ẫ', "aax", "a64", "a^~"),
    ('Ậ', "Aaj", "A65", "A^."),
    ('ậ', "aaj", "a65", "a^."),
    ('Ắ', "Aws", "A81", "A('"),
    ('ắ', "aws", "a81", "a('"),
    ('Ằ', "Awf", "A82", "A(`"),
    ('ằ', "awf", "a82", "a(`"),
    ('Ẳ', "Awr", "A83", "A(?"),
    ('ẳ', "awr", "a83", "a(?"),
    ('Ẵ', "Awx", "A84", "A(~"),
    ('ẵ', "awx", "a84", "a(~"),
    ('Ặ', "Awj", "A85", "A(."),
    ('ặ', "awj", "a85", "a(."),
    ('Ẹ', "Ej", "E5", "E."),
    ('ẹ', "ej", "e5", "e."),
    ('Ẻ', "Er", "E3", "E?"),
    ('ẻ', "er", "e3", "e?"),
    ('Ẽ', "Ex", "E4", "E~"),
    ('ẽ', "ex", "e4", "e~"),
    ('Ế', "Ees", "E61", "E^'"),
    ('ế', "ees", "e61", "e^'"),
    ('Ề', "Eef", "E62", "E^`"),
    ('ề', "eef", "e62", "e^`"),
    ('Ể', "Eer", "E63", "E^?"),
    ('ể', "eer", "e63", "e^?"),
    ('Ễ', "Eex", "E64", "E^~"),
    ('ễ', "eex", "e64", "e^~"),
    ('Ệ', "Eej", "E65", "E^."),
    ('ệ', "eej", "e65", "e^."),
    ('Ỉ', "Ir", "I3", "I?"),
    ('ỉ', "ir", "i3", "i?"),
    ('Ị', "Ij", "I5", "I."),
    ('ị', "ij", "i5", "i."),
    ('Ọ', "Oj", "O5", "O."),
    ('ọ', "oj", "o5", "o."),
    ('Ỏ', "Or", "O3", "O?"),
    ('ỏ', "or", "o3", "o?"),
    ('Ố', "Oos", "O61", "O^'"),
    ('ố', "oos", "o61", "o^'"),
    ('Ồ', "Oof", "O62", "O^`"),
    ('ồ', "oof", "o62", "o^`"),
    ('Ổ', "Oor", "O63", "O^?"),
    ('ổ', "oor", "o63", "o^?"),
    ('Ỗ', "Oox", "O64", "O^~"),
    ('ỗ', "oox", "o64", "o^~"),
    ('Ộ', "Ooj", "O65", "O^."),
    ('ộ', "ooj", "o65", "o^."),
    ('Ớ', "Ows", "O71", "O+'"),
    ('ớ', "ows", "o71", "o+'"),
    ('Ờ', "Owf", "O72", "O+`"),
    ('ờ', "owf", "o72", "o+`"),
    ('Ở', "Owr", "O73", "O+?"),
    ('ở', "owr", "o73", "o+?"),
    ('Ỡ', "Owx", "O74", "O+~"),
    ('ỡ', "owx", "o74", "o+~"),
    ('Ợ', "Owj", "O75", "O+."),
    ('ợ', "owj", "o75", "o+."),
    ('Ụ', "Uj", "U5", "U."),
    ('ụ', "uj", "u5", "u."),
    ('Ủ', "Ur", "U3", "U?"),
    ('ủ', "ur", "u3", "u?"),
    ('Ứ', "Uws", "U71", "U+'"),
    ('ứ', "uws", "u71", "u+'"),
    ('Ừ', "Uwf", "U72", "U+`"),
    ('ừ', "uwf", "u72", "u+`"),
    ('Ử', "Uwr", "U73", "U+?"),
    ('ử', "uwr", "u73", "u+?"),
    ('Ữ', "Uwx", "U74", "U+~"),
    ('ữ', "uwx", "u74", "u+~"),
    ('Ự', "Uwj", "U75", "U+."),
    ('ự', "uwj", "u75", "u+."),
    ('Ỳ', "Yf", "Y2", "Y`"),
    ('ỳ', "yf", "y2", "y`"),
    ('Ỵ', "Yj", "Y5", "Y."),
    ('ỵ', "yj", "y5", "y."),
    ('Ỷ', "Yr", "Y3", "Y?"),
    ('ỷ', "yr", "y3", "y?"),
    ('Ỹ', "Yx", "Y4", "Y~"),
    ('ỹ', "yx", "y4", "y~"),
];

pub(super) fn chars_from_composed(ch: char, method: InputMethod) -> &'static str {
    if let Ok(i) = GLYPHS.binary_search_by_key(&ch, |row| row.0) {
        return match method {
            InputMethod::Telex => GLYPHS[i].1,
            InputMethod::Vni => GLYPHS[i].2,
            InputMethod::Viqr => GLYPHS[i].3,
        };
    }
    ascii_passthrough(ch)
}

fn ascii_passthrough(ch: char) -> &'static str {
    const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
    const UPPER: [&str; 26] = [
        "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M",
        "N", "O", "P", "Q", "R", "S", "T", "U", "V", "W", "X", "Y", "Z",
    ];
    const LOWER: [&str; 26] = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m",
        "n", "o", "p", "q", "r", "s", "t", "u", "v", "w", "x", "y", "z",
    ];
    match ch {
        '0'..='9' => DIGITS[(ch as u8 - b'0') as usize],
        'A'..='Z' => UPPER[(ch as u8 - b'A') as usize],
        'a'..='z' => LOWER[(ch as u8 - b'a') as usize],
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_are_sorted_for_binary_search() {
        assert!(GLYPHS.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn method_columns_and_ascii_passthrough() {
        assert_eq!(chars_from_composed('á', InputMethod::Telex), "as");
        assert_eq!(chars_from_composed('á', InputMethod::Vni), "a1");
        assert_eq!(chars_from_composed('á', InputMethod::Viqr), "a'");
        assert_eq!(chars_from_composed('ơ', InputMethod::Viqr), "o+");
        assert_eq!(chars_from_composed('ô', InputMethod::Viqr), "o^");
        assert_eq!(chars_from_composed('ă', InputMethod::Viqr), "a(");
        assert_eq!(chars_from_composed('a', InputMethod::Telex), "a");
        assert_eq!(chars_from_composed('7', InputMethod::Vni), "7");
        assert_eq!(chars_from_composed(' ', InputMethod::Telex), "");
    }
}
