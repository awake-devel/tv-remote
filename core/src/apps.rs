//! Finding the installed apps.
//!
//! Tizen 2025 firmware ignores `ed.installedApp.get` from third-party clients (or answers
//! with a short partner list), so [`crate::Tv::installed_apps`] asks REST
//! (`GET /api/v2/applications/<id>`, 200 = installed, 404 = not) about every id of
//! [`KNOWN_APPS`]. Apps outside the catalog cannot be listed: add their id here, or pass
//! it as an extra id ([`crate::Tv::installed_apps_with`]).

use crate::remote::App;

/// Tizen app ids probed by [`crate::Tv::installed_apps`] (id, fallback name; the TV's own
/// name wins). Built from the community lists of samsung-tv-ws-api and
/// homebridge-samsung-tizen plus ids checked on a 2025 Q7F (France). Ids differ by TV year
/// and region, so several ids per app is normal; unknown ids only cost a 404.
pub const KNOWN_APPS: &[(&str, &str)] = &[
    ("3201704012147", "10 play"),
    ("3201905018499", "13 Now"),
    ("3201803015934", "7plus"),
    ("3201607010031", "9Now"),
    ("3201812017479", "ABC iview"),
    ("111299002148", "All 4"),
    ("3202004020626", "Amazon Alexa"),
    ("3202203026819", "Amazon Luna"),
    ("3201710014874", "Amazon Music"),
    ("3201811017337", "AMC"),
    ("3201611011005", "AntenaPlay.ro"),
    ("3201908019041", "Apple Music"),
    ("3201807016597", "Apple TV"),
    ("3201412000679", "ARD Mediathek"),
    ("3202011022316", "ARTE"),
    ("3201910019420", "b.tv"),
    ("3201601007670", "BBC iPlayer"),
    ("3201602007865", "BBC News"),
    ("3202003020365", "BBC Sounds"),
    ("3202007021336", "Benshi"),
    ("3202010022098", "BINGE"),
    ("3201705012375", "Blackdove Video Art"),
    ("3201412000690", "Bloomberg"),
    ("3201909019175", "BritBox"),
    ("3201803015869", "Canaal Digitaal"),
    ("3201606009910", "CANAL+"),
    ("3202010022094", "CANAL+"),
    ("3202503039231", "Copilot"),
    ("3201506003488", "Crave"),
    ("3201506003486", "CTV"),
    ("3201806016390", "DAZN"),
    ("3201608010191", "Deezer"),
    ("3202205027432", "DigiOnline"),
    ("3201907018786", "DIRECTV GO"),
    ("3201803015944", "Discovery+"),
    ("3201901017640", "Disney+"),
    ("3202009021709", "Disney+"),
    ("3202204027038", "Disney+"),
    ("3201804016109", "DStv"),
    ("3201608010385", "EduPedia"),
    ("3201606009872", "Emby"),
    ("3201708014618", "ESPN"),
    ("3201712015454", "Estadio TNT Sports"),
    ("3201703012079", "Eurosport Player"),
    ("3202004020674", "Explore Google Assistant"),
    ("11091000000", "Facebook Watch"),
    ("3201906018693", "Focus Sat"),
    ("3201910019449", "Foxtel"),
    ("3201911019629", "france.tv"),
    ("3202103023232", "france.tv"),
    ("3202103023185", "Free TV"),
    ("3201710015037", "Gallery"),
    ("3201908019022", "globoplay"),
    ("3202008021439", "Google Meet"),
    ("3201806016381", "hayu"),
    ("3201706012478", "HBO GO"),
    ("3202004020575", "HBO GO Asia"),
    ("3201601007230", "HBO Max"),
    ("3201810017070", "HD+"),
    ("3201601007625", "Hulu"),
    ("3201907018784", "Internet"),
    ("3202010022079", "Internet"),
    ("org.tizen.browser", "Internet"),
    ("3201812017585", "IPTV Smarters"),
    ("3202009021808", "IPTV Smarters Pro"),
    ("3202006020934", "iQIYI"),
    ("121299000089", "ITV Hub"),
    ("3201910019513", "iWantTFC"),
    ("3201502001386", "Joyn"),
    ("3201803015997", "Kan11"),
    ("3201910019354", "Kayo Sports"),
    ("3201910019457", "Kidoodle.TV"),
    ("3201901017768", "Kijk"),
    ("3201906018702", "Kodi"),
    ("3201803015963", "KPN iTV"),
    ("3201703012065", "Love Nature 4K"),
    ("3201801015366", "M6+"),
    ("3202207027908", "M6+"),
    ("3201907018746", "MagentaTV"),
    ("3201506003716", "Mako"),
    ("3202301029760", "Max"),
    ("3201612011418", "McAfee Security"),
    ("3201706014259", "Mi Perro Chocolo"),
    ("3201603008210", "MLB"),
    ("3201611011210", "Molotov"),
    ("3201709014773", "Molotov"),
    ("111299001957", "Movistar TV Chile"),
    ("121299000612", "My5"),
    ("11101200001", "Netflix"),
    ("3201907018807", "Netflix"),
    ("3202201026324", "Nifty Gateway"),
    ("3202012022421", "NL Ziet"),
    ("3202011022131", "NOW PlayTV"),
    ("3201603008746", "NOW TV"),
    ("3201706014250", "NPO"),
    ("3201703012029", "OCS"),
    ("3202105023867", "Orange TV"),
    ("3201710014866", "Orange TV Go"),
    ("3201710014981", "Paramount+"),
    ("3202110025305", "Paramount+"),
    ("3201506003175", "Pathe Thuis"),
    ("3202311033030", "Pathé Home"),
    ("3202006020991", "Peacock TV"),
    ("3201810017091", "Playzer"),
    ("3201512006963", "Plex"),
    ("3201808016802", "Pluto TV"),
    ("3201512006785", "Prime Video"),
    ("3201910019365", "Prime Video"),
    ("3201909019271", "PrivacyChoices"),
    ("AntenaPlay.ro", "PrivacyChoices"),
    ("3201711015226", "Radio UK"),
    ("3202012022468", "Radio WOW"),
    ("111399002034", "RaiPlay"),
    ("3201511006428", "Rakuten TV"),
    ("3201602007756", "Red Bull TV"),
    ("3201812017600", "Reshet13"),
    ("3202303030351", "RMC BFM Play"),
    ("3201704012212", "RMC Sport"),
    ("3201906018642", "RTL XL (nieuw)"),
    ("3201908018988", "RTL+"),
    ("3202405035480", "Samsung TV Plus"),
    ("3201510005981", "SBS On Demand"),
    ("3202009021877", "Security Center"),
    ("3201506002963", "Showmax"),
    ("3201903018045", "Showmax"),
    ("3201902017887", "Shudder"),
    ("111399002220", "SiriusXM"),
    ("3202208028071", "SkyShowtime"),
    ("3202102022877", "SmartOne IPTV"),
    ("3201710015016", "SmartThings"),
    ("3201712015217", "SmartThings"),
    ("3201910019378", "SmartThings"),
    ("3202311033054", "SmartThings"),
    ("3201606009684", "Spotify"),
    ("3201606009798", "Stan"),
    ("3201702011851", "Steam Link"),
    ("3201604009182", "Telecine"),
    ("3202008021462", "Telefoot"),
    ("11101000407", "Telstra TV Box Office"),
    ("3201712015183", "TF1+"),
    ("3202209028524", "TF1+"),
    ("3202204027037", "The Austria Channel"),
    ("111399000741", "The Weather Network"),
    ("3201805016367", "TIDAL"),
    ("3202008021577", "TikTok"),
    ("3202008021516", "TMobile TV"),
    ("3201504001965", "Tubi Free Movies ＆ TV"),
    ("121299000101", "TuneIn"),
    ("3202203026841", "Twitch"),
    ("3201806016432", "UKTV Play"),
    ("3201710015067", "Universal Guide"),
    ("11111300404", "Viaplay"),
    ("3201810017074", "Videoland"),
    ("3201807016555", "Viu"),
    ("111299000769", "VOYO.RO"),
    ("111012010001", "VUDU"),
    ("3202203026723", "Xbox"),
    ("3202203026799", "Xbox"),
    ("111299001912", "YouTube"),
    ("3201611010983", "YouTube Kids"),
    ("3201707014489", "YouTube TV"),
    ("3201705012365", "ZDF Mediathek"),
];

/// How many REST probes run at once.
pub const PROBE_PARALLELISM: usize = 16;

/// The ids to probe: [`KNOWN_APPS`] then `extra` (trimmed, non-empty), without duplicates. Pure.
pub fn probe_ids(extra: &[&str]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let known = KNOWN_APPS.iter().map(|&(id, name)| (id.to_string(), name.to_string()));
    let extra = extra.iter().map(|id| id.trim()).filter(|id| !id.is_empty()).map(|id| (id.to_string(), id.to_string()));
    for (id, name) in known.chain(extra) {
        if !out.iter().any(|(i, _)| *i == id) {
            out.push((id, name));
        }
    }
    out
}

/// One list from several sources: the first app with an id wins; sorted by name (any
/// case), then id. Pure.
pub fn merge_apps(lists: Vec<Vec<App>>) -> Vec<App> {
    let mut out: Vec<App> = Vec::new();
    for app in lists.into_iter().flatten() {
        if !out.iter().any(|a| a.app_id == app.app_id) {
            out.push(app);
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.app_id.cmp(&b.app_id)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str) -> App {
        App { app_id: id.into(), name: name.into(), app_type: 2, icon: None }
    }

    #[test]
    fn catalog_is_clean() {
        assert!(KNOWN_APPS.len() > 100);
        for (i, (id, name)) in KNOWN_APPS.iter().enumerate() {
            assert!(!id.is_empty() && !id.contains(' ') && !name.is_empty(), "{id:?} {name:?}");
            assert!(!KNOWN_APPS[..i].iter().any(|(o, _)| o == id), "duplicate {id}");
        }
        // Checked on a 2025 Q7F: these must stay.
        for id in ["111299001912", "3201907018807", "3201611011210", "3202011022316", "3202103023232",
                   "3202207027908", "3202209028524", "3202311033030", "3201910019420"] {
            assert!(KNOWN_APPS.iter().any(|(i, _)| *i == id), "{id}");
        }
    }

    #[test]
    fn extra_ids_are_added_once() {
        let ids = probe_ids(&[" 3209999999999 ", "", "111299001912", "3209999999999"]);
        assert_eq!(ids.len(), KNOWN_APPS.len() + 1);
        assert_eq!(ids.last().unwrap(), &("3209999999999".to_string(), "3209999999999".to_string()));
    }

    #[test]
    fn merge_dedupes_and_sorts() {
        let m = merge_apps(vec![
            vec![app("2", "youtube"), app("1", "Netflix")],
            vec![app("1", "Netflix (other name)"), app("3", "arte")],
        ]);
        let names: Vec<_> = m.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["arte", "Netflix", "youtube"]);
    }
}
