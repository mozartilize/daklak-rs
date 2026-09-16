//! Composed-glyph reversal tables shared by Telex, VNI, and VIQR.

use super::InputMethod;

pub(super) const GLYPHS: &[(char, &str, &str, &str)] = &[
    ('À', "AF", "A2", "A`"),
    ('Á', "AS", "A1", "A'"),
    ('Â', "AA", "A6", "A^"),
    ('Ã', "AX", "A4", "A~"),
    ('È', "EF", "E2", "E`"),
    ('É', "ES", "E1", "E'"),
    ('Ê', "EE", "E6", "E^"),
    ('Ì', "IF", "I2", "I`"),
    ('Í', "IS", "I1", "I'"),
    ('Ò', "OF", "O2", "O`"),
    ('Ó', "OS", "O1", "O'"),
    ('Ô', "OO", "O6", "O^"),
    ('Õ', "OX", "O4", "O~"),
    ('Ù', "UF", "U2", "U`"),
    ('Ú', "US", "U1", "U'"),
    ('Ý', "YS", "Y1", "Y'"),
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
    ('Ă', "AW", "A8", "A("),
    ('ă', "aw", "a8", "a("),
    ('Đ', "DD", "D9", "DD"),
    ('đ', "dd", "d9", "dd"),
    ('Ĩ', "IX", "I4", "I~"),
    ('ĩ', "ix", "i4", "i~"),
    ('Ũ', "UX", "U4", "U~"),
    ('ũ', "ux", "u4", "u~"),
    ('Ơ', "OW", "O7", "O+"),
    ('ơ', "ow", "o7", "o+"),
    ('Ư', "UW", "U7", "U+"),
    ('ư', "uw", "u7", "u+"),
    ('Ạ', "AJ", "A5", "A."),
    ('ạ', "aj", "a5", "a."),
    ('Ả', "AR", "A3", "A?"),
    ('ả', "ar", "a3", "a?"),
    ('Ấ', "AAS", "A61", "A^'"),
    ('ấ', "aas", "a61", "a^'"),
    ('Ầ', "AAF", "A62", "A^`"),
    ('ầ', "aaf", "a62", "a^`"),
    ('Ẩ', "AAR", "A63", "A^?"),
    ('ẩ', "aar", "a63", "a^?"),
    ('Ẫ', "AAX", "A64", "A^~"),
    ('ẫ', "aax", "a64", "a^~"),
    ('Ậ', "AAJ", "A65", "A^."),
    ('ậ', "aaj", "a65", "a^."),
    ('Ắ', "AWS", "A81", "A('"),
    ('ắ', "aws", "a81", "a('"),
    ('Ằ', "AWF", "A82", "A(`"),
    ('ằ', "awf", "a82", "a(`"),
    ('Ẳ', "AWR", "A83", "A(?"),
    ('ẳ', "awr", "a83", "a(?"),
    ('Ẵ', "AWX", "A84", "A(~"),
    ('ẵ', "awx", "a84", "a(~"),
    ('Ặ', "AWJ", "A85", "A(."),
    ('ặ', "awj", "a85", "a(."),
    ('Ẹ', "EJ", "E5", "E."),
    ('ẹ', "ej", "e5", "e."),
    ('Ẻ', "ER", "E3", "E?"),
    ('ẻ', "er", "e3", "e?"),
    ('Ẽ', "EX", "E4", "E~"),
    ('ẽ', "ex", "e4", "e~"),
    ('Ế', "EES", "E61", "E^'"),
    ('ế', "ees", "e61", "e^'"),
    ('Ề', "EEF", "E62", "E^`"),
    ('ề', "eef", "e62", "e^`"),
    ('Ể', "EER", "E63", "E^?"),
    ('ể', "eer", "e63", "e^?"),
    ('Ễ', "EEX", "E64", "E^~"),
    ('ễ', "eex", "e64", "e^~"),
    ('Ệ', "EEJ", "E65", "E^."),
    ('ệ', "eej", "e65", "e^."),
    ('Ỉ', "IR", "I3", "I?"),
    ('ỉ', "ir", "i3", "i?"),
    ('Ị', "IJ", "I5", "I."),
    ('ị', "ij", "i5", "i."),
    ('Ọ', "OJ", "O5", "O."),
    ('ọ', "oj", "o5", "o."),
    ('Ỏ', "OR", "O3", "O?"),
    ('ỏ', "or", "o3", "o?"),
    ('Ố', "OOS", "O61", "O^'"),
    ('ố', "oos", "o61", "o^'"),
    ('Ồ', "OOF", "O62", "O^`"),
    ('ồ', "oof", "o62", "o^`"),
    ('Ổ', "OOR", "O63", "O^?"),
    ('ổ', "oor", "o63", "o^?"),
    ('Ỗ', "OOX", "O64", "O^~"),
    ('ỗ', "oox", "o64", "o^~"),
    ('Ộ', "OOJ", "O65", "O^."),
    ('ộ', "ooj", "o65", "o^."),
    ('Ớ', "OWS", "O71", "O+'"),
    ('ớ', "ows", "o71", "o+'"),
    ('Ờ', "OWF", "O72", "O+`"),
    ('ờ', "owf", "o72", "o+`"),
    ('Ở', "OWR", "O73", "O+?"),
    ('ở', "owr", "o73", "o+?"),
    ('Ỡ', "OWX", "O74", "O+~"),
    ('ỡ', "owx", "o74", "o+~"),
    ('Ợ', "OWJ", "O75", "O+."),
    ('ợ', "owj", "o75", "o+."),
    ('Ụ', "UJ", "U5", "U."),
    ('ụ', "uj", "u5", "u."),
    ('Ủ', "UR", "U3", "U?"),
    ('ủ', "ur", "u3", "u?"),
    ('Ứ', "UWS", "U71", "U+'"),
    ('ứ', "uws", "u71", "u+'"),
    ('Ừ', "UWF", "U72", "U+`"),
    ('ừ', "uwf", "u72", "u+`"),
    ('Ử', "UWR", "U73", "U+?"),
    ('ử', "uwr", "u73", "u+?"),
    ('Ữ', "UWX", "U74", "U+~"),
    ('ữ', "uwx", "u74", "u+~"),
    ('Ự', "UWJ", "U75", "U+."),
    ('ự', "uwj", "u75", "u+."),
    ('Ỳ', "YF", "Y2", "Y`"),
    ('ỳ', "yf", "y2", "y`"),
    ('Ỵ', "YJ", "Y5", "Y."),
    ('ỵ', "yj", "y5", "y."),
    ('Ỷ', "YR", "Y3", "Y?"),
    ('ỷ', "yr", "y3", "y?"),
    ('Ỹ', "YX", "Y4", "Y~"),
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
        assert_eq!(chars_from_composed('a', InputMethod::Telex), "a");
        assert_eq!(chars_from_composed('7', InputMethod::Vni), "7");
        assert_eq!(chars_from_composed(' ', InputMethod::Telex), "");
    }
}
