//! RTF/CRE dictionaries, placeholder until the reader is written.

/// The outlines and translations of an RTF/CRE dictionary, in Plover's translation language.
pub fn parse_rtf(_bytes: &[u8]) -> anyhow::Result<Vec<(String, String)>> {
    anyhow::bail!("RTF/CRE dictionaries are not supported yet")
}
