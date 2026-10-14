use kmb::uni::Description;

const NAME_MAX: usize = 30;

fn push_name(out: &mut Vec<u8>, name: &str) {
    let bytes = name.as_bytes();
    let cut = bytes.get(..NAME_MAX).unwrap_or(bytes);
    out.extend_from_slice(cut);
    if cut.len() < NAME_MAX {
        out.push(0);
    }
}

pub(crate) fn blocks(blocks: &[&[u8]]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for block in blocks {
        out.extend_from_slice(&u16::try_from(block.len()).ok()?.to_be_bytes());
        out.extend_from_slice(block);
    }
    Some(out)
}

pub fn try_description_blob(d: &Description) -> Result<Vec<u8>, kmb::DecodeError> {
    let mut a = Vec::new();
    for g in &d.groups {
        a.push(g.idx);
        a.push(g.idxs);
        push_name(&mut a, &g.name);
    }
    a.push(0xFF);
    let mut b = Vec::new();
    for f in &d.fields {
        push_name(&mut b, &f.name);
        b.push(f.ic);
        b.push(f.type_code);
        b.extend_from_slice(&f.pb.to_be_bytes());
        b.push(f.store);
    }
    blocks(&[&a, &b]).ok_or(kmb::DecodeError::Invalid {
        structure: "config description",
        reason: "a list is over 65535 bytes",
    })
}

pub fn data_blob(d: &Description) -> Result<Vec<u8>, kmb::DecodeError> {
    let section = |ic: u8| {
        d.groups
            .iter()
            .find(|g| g.idx == ic)
            .map(|g| g.name.as_str())
    };
    let in_io = |name: &str| {
        d.fields
            .iter()
            .any(|f| f.name == name && section(f.ic) == Some("InputOutput"))
    };
    let zero_hm_d = !in_io("HM_D2") && in_io("HM_D");
    let mut out = Vec::new();
    for (f, v) in d.fields.iter().zip(&d.values) {
        if f.store == 0 {
            continue;
        }
        let v = v.as_ref().ok_or(kmb::DecodeError::Invalid {
            structure: "config value",
            reason: "a stored item has no value",
        })?;
        let start = out.len();
        v.write(&mut out, f.typ(), f.pb)?;
        if zero_hm_d
            && f.name == "HM_D"
            && section(f.ic) == Some("InputOutput")
            && let Some(field) = out.get_mut(start..)
        {
            for element in field.chunks_mut(19) {
                for b in element.iter_mut().take(13) {
                    *b = 0;
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_lengths_are_never_wrong() {
        assert_eq!(blocks(&[&[1, 2], &[]]), Some(vec![0, 2, 1, 2, 0, 0]),);
        let max = vec![0_u8; usize::from(u16::MAX)];
        assert_eq!(blocks(&[&max]).map(|b| b.len()), Some(65_537));
        let over = vec![0_u8; 65_536];
        assert_eq!(blocks(&[&[1], &over]), None);
    }
}
