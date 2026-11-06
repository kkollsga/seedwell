//! The torrent proxy's country (SPEC §7H): the user picks where they are, by hand (Seedwell never detects it), and
//! the server choice prefers servers in that country, then in the nearest countries. Distances are between
//! approximate country centres: enough to tell next door from another continent, which is all the choice needs.

/// A country the user can pick: ISO 3166-1 alpha-2 code, English name, approximate centre (latitude, longitude).
pub struct Country {
    pub code: &'static str,
    pub name: &'static str,
    pub lat: f32,
    pub lon: f32,
}

const fn c(code: &'static str, name: &'static str, lat: f32, lon: f32) -> Country {
    Country { code, name, lat, lon }
}

/// The countries to choose from, by name.
pub const COUNTRIES: [Country; 58] = [
    c("AR", "Argentina", -34.0, -64.0),
    c("AU", "Australia", -25.0, 134.0),
    c("AT", "Austria", 47.5, 14.5),
    c("BE", "Belgium", 50.6, 4.6),
    c("BR", "Brazil", -10.0, -52.0),
    c("BG", "Bulgaria", 42.7, 25.3),
    c("CA", "Canada", 56.0, -96.0),
    c("CL", "Chile", -33.0, -71.0),
    c("CN", "China", 35.0, 104.0),
    c("CO", "Colombia", 4.0, -73.0),
    c("HR", "Croatia", 45.1, 15.2),
    c("CZ", "Czechia", 49.8, 15.5),
    c("DK", "Denmark", 56.0, 10.0),
    c("EE", "Estonia", 58.6, 25.0),
    c("FI", "Finland", 64.0, 26.0),
    c("FR", "France", 46.6, 2.4),
    c("DE", "Germany", 51.2, 10.4),
    c("GR", "Greece", 39.1, 22.0),
    c("HK", "Hong Kong", 22.3, 114.2),
    c("HU", "Hungary", 47.2, 19.5),
    c("IS", "Iceland", 64.9, -18.6),
    c("IN", "India", 22.0, 79.0),
    c("ID", "Indonesia", -2.5, 118.0),
    c("IE", "Ireland", 53.4, -8.2),
    c("IL", "Israel", 31.0, 35.0),
    c("IT", "Italy", 42.8, 12.6),
    c("JP", "Japan", 36.2, 138.3),
    c("LV", "Latvia", 56.9, 24.6),
    c("LT", "Lithuania", 55.2, 23.9),
    c("LU", "Luxembourg", 49.8, 6.1),
    c("MY", "Malaysia", 4.2, 102.0),
    c("MX", "Mexico", 23.6, -102.5),
    c("NL", "Netherlands", 52.1, 5.3),
    c("NZ", "New Zealand", -41.0, 174.0),
    c("NO", "Norway", 61.0, 9.0),
    c("PH", "Philippines", 12.9, 121.8),
    c("PL", "Poland", 52.0, 19.1),
    c("PT", "Portugal", 39.6, -8.0),
    c("RO", "Romania", 45.9, 25.0),
    c("RS", "Serbia", 44.0, 20.9),
    c("SG", "Singapore", 1.35, 103.8),
    c("SK", "Slovakia", 48.7, 19.7),
    c("SI", "Slovenia", 46.1, 14.8),
    c("ZA", "South Africa", -29.0, 24.0),
    c("KR", "South Korea", 36.5, 127.9),
    c("ES", "Spain", 40.2, -3.7),
    c("SE", "Sweden", 62.0, 15.0),
    c("CH", "Switzerland", 46.8, 8.2),
    c("TW", "Taiwan", 23.7, 121.0),
    c("TH", "Thailand", 15.0, 101.0),
    c("TR", "Türkiye", 39.0, 35.2),
    c("UA", "Ukraine", 49.0, 31.4),
    c("AE", "United Arab Emirates", 24.0, 54.0),
    c("GB", "United Kingdom", 54.0, -2.0),
    c("US", "United States", 39.8, -98.6),
    c("VN", "Vietnam", 16.0, 107.8),
    c("EG", "Egypt", 26.8, 30.8),
    c("KE", "Kenya", 0.0, 38.0),
];

/// The country with this code (any case).
pub fn country(code: &str) -> Option<&'static Country> {
    COUNTRIES.iter().find(|c| c.code.eq_ignore_ascii_case(code.trim()))
}

/// Kilometres between two countries' centres (great circle).
fn km(a: &Country, b: &Country) -> f64 {
    let (la1, lo1, la2, lo2) = (f64::from(a.lat).to_radians(), f64::from(a.lon).to_radians(), f64::from(b.lat).to_radians(), f64::from(b.lon).to_radians());
    let h = ((la2 - la1) / 2.0).sin().powi(2) + la1.cos() * la2.cos() * ((lo2 - lo1) / 2.0).sin().powi(2);
    6371.0 * 2.0 * h.sqrt().asin()
}

/// How much each server is preferred for a user in `home` (0 first), from the servers' countries: the user's
/// country first, then the others by distance. Empty without a (known) home country: no preference.
pub fn preference(home: &str, server_countries: &[&str]) -> Vec<u16> {
    let Some(home) = country(home) else { return Vec::new() };
    let mut order: Vec<(f64, &str)> = Vec::new();
    for code in server_countries {
        if order.iter().any(|(_, c)| c.eq_ignore_ascii_case(code)) {
            continue;
        }
        let d = if code.eq_ignore_ascii_case(home.code) { -1.0 } else { country(code).map_or(f64::MAX, |c| km(home, c)) };
        order.push((d, code));
    }
    order.sort_by(|a, b| a.0.total_cmp(&b.0));
    server_countries
        .iter()
        .map(|code| order.iter().position(|(_, c)| c.eq_ignore_ascii_case(code)).unwrap_or(order.len()) as u16)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Failure: a user next to one server country is steered to a far one first, or a server in the user's own
    /// country is not first.
    #[test]
    fn home_first_then_the_nearest_countries() {
        let servers = ["NL", "SE", "US", "NL"];
        assert_eq!(preference("NO", &servers), [1, 0, 2, 1], "Norway: Sweden, then the Netherlands, then the US");
        assert_eq!(preference("de", &servers), [0, 1, 2, 0], "Germany: the Netherlands first");
        assert_eq!(preference("US", &servers)[2], 0, "the US: its own first");
        assert_eq!(preference("CA", &servers)[2], 0, "Canada: the US first");
        assert_eq!(preference("AU", &servers).iter().filter(|&&p| p == 0).count(), 1, "far from all: still one first");
        assert!(preference("", &servers).is_empty() && preference("XX", &servers).is_empty(), "no preference");
        let mut codes: Vec<&str> = COUNTRIES.iter().map(|c| c.code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), COUNTRIES.len(), "no country twice");
    }
}
