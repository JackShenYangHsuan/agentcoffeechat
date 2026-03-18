use rand::seq::SliceRandom;

/// 256 common, easy-to-say English words used for three-word pairing codes.
const WORDLIST: [&str; 256] = [
    "apple",    "anchor",   "arrow",    "atlas",    "autumn",   "badge",    "bamboo",   "barrel",
    "beacon",   "birch",    "blade",    "bloom",    "bluff",    "bolt",     "bonus",    "brave",
    "breeze",   "bridge",   "brook",    "bronze",   "brush",    "cabin",    "candle",   "canyon",
    "cargo",    "castle",   "cedar",    "chalk",    "charm",    "cider",    "cliff",    "clock",
    "cloud",    "clover",   "comet",    "copper",   "coral",    "crane",    "creek",    "crown",
    "crystal",  "dagger",   "dawn",     "delta",    "denim",    "drift",    "drum",     "dusk",
    "eagle",    "earth",    "echo",     "elder",    "ember",    "epoch",    "fable",    "falcon",
    "fern",     "ferry",    "field",    "flare",    "flame",    "flint",    "forge",    "frost",
    "garden",   "garnet",   "gate",     "geyser",   "ginger",   "glacier",  "glade",    "globe",
    "golden",   "grain",    "granite",  "grove",    "harbor",   "hatch",    "haven",    "hawk",
    "hazel",    "heath",    "heron",    "hilltop",  "hollow",   "honey",    "horizon",  "humble",
    "iron",     "ivory",    "ivy",      "jade",     "jasmine",  "jazz",     "jewel",    "journal",
    "jungle",   "kayak",    "kernel",   "kettle",   "kindle",   "kite",     "knight",   "knoll",
    "lagoon",   "lantern",  "larch",    "lava",     "legend",   "lemon",    "light",    "linen",
    "lunar",    "marble",   "marsh",    "mason",    "meadow",   "mesa",     "meteor",   "mist",
    "moose",    "mosaic",   "noble",    "north",    "nova",     "nutmeg",   "oasis",    "ocean",
    "olive",    "onyx",     "orbit",    "orchid",   "otter",    "palm",     "panther",  "parcel",
    "patrol",   "pearl",    "pebble",   "pepper",   "piano",    "pilot",    "pine",     "pixel",
    "plume",    "pond",     "prism",    "pulse",    "quartz",   "quest",    "quill",    "radiant",
    "raven",    "reef",     "ridge",    "ripple",   "river",    "robin",    "rocket",   "ruby",
    "saddle",   "sage",     "salmon",   "sand",     "satin",    "scout",    "shell",    "sierra",
    "silk",     "silver",   "sketch",   "slate",    "snow",     "solar",    "spark",    "spire",
    "spruce",   "stanza",   "starling", "storm",    "summit",   "swift",    "talon",    "tango",
    "temple",   "terra",    "thistle",  "thunder",  "tide",     "tiger",    "timber",   "torch",
    "trail",    "tropic",   "tulip",    "tundra",   "ultra",    "unity",    "valley",   "vapor",
    "velvet",   "venture",  "violet",   "vista",    "vivid",    "walnut",   "wander",   "whisper",
    "willow",   "winter",   "wolf",     "wonder",   "xenon",    "yacht",    "yarn",     "yellow",
    "zenith",   "zephyr",   "zinc",     "acorn",    "alpine",   "aspen",    "basalt",   "bison",
    "blaze",    "brine",    "canopy",   "cove",     "dune",     "elm",      "flock",    "gorge",
    "haze",     "inlet",    "jasper",   "kelp",     "loft",     "maple",    "nimbus",   "oak",
    "peak",     "quail",    "rift",     "shore",    "stone",    "thyme",    "umber",    "vine",
    "wave",     "wren",     "yew",      "fjord",    "plaza",    "cloak",    "zero",     "petal",
];

/// Generate a three-word pairing code (e.g. "frost-meadow-tiger").
///
/// Picks three distinct random words from the 256-word list and joins them with
/// hyphens.
pub fn generate_three_word_code() -> String {
    let mut rng = rand::thread_rng();
    let words: Vec<&str> = WORDLIST.choose_multiple(&mut rng, 3).copied().collect();
    words.join("-")
}

/// Validate that `code` consists of exactly three hyphen-separated words, each
/// present in the wordlist.
pub fn validate_code(code: &str) -> bool {
    let parts: Vec<&str> = code.split('-').collect();
    if parts.len() != 3 {
        return false;
    }
    parts.iter().all(|word| WORDLIST.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_has_three_words() {
        let code = generate_three_word_code();
        let parts: Vec<&str> = code.split('-').collect();
        assert_eq!(parts.len(), 3, "Code should have exactly 3 words: {code}");
    }

    #[test]
    fn all_words_from_wordlist() {
        for _ in 0..20 {
            let code = generate_three_word_code();
            for word in code.split('-') {
                assert!(
                    WORDLIST.contains(&word),
                    "Word '{word}' not found in wordlist"
                );
            }
        }
    }

    #[test]
    fn successive_codes_differ() {
        // With 256^3 possibilities collisions are astronomically unlikely.
        let codes: Vec<String> = (0..10).map(|_| generate_three_word_code()).collect();
        let unique: std::collections::HashSet<&String> = codes.iter().collect();
        assert!(
            unique.len() > 1,
            "Expected different codes across 10 calls"
        );
    }

    #[test]
    fn validate_accepts_valid_code() {
        let code = generate_three_word_code();
        assert!(
            validate_code(&code),
            "Generated code should validate: {code}"
        );
    }

    #[test]
    fn validate_rejects_wrong_word_count() {
        assert!(!validate_code("apple-bridge"));
        assert!(!validate_code("apple-bridge-castle-delta"));
        assert!(!validate_code("apple"));
        assert!(!validate_code(""));
    }

    #[test]
    fn validate_rejects_unknown_words() {
        assert!(!validate_code("apple-bridge-xylophonez"));
        assert!(!validate_code("foo-bar-baz"));
    }

    #[test]
    fn validate_rejects_malformed_separators() {
        assert!(!validate_code("apple bridge castle"));
        assert!(!validate_code("apple_bridge_castle"));
    }
}
