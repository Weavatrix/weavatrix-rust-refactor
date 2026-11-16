//! A written Rust type rendered back to source.
//!
//! Only paths without generic arguments, references, slices, and the unit type are rendered.
//! Anything else is refused: guessing a lifetime or a bound would invent a helper the file
//! does not actually type-check as.

#[must_use]
pub(super) fn render_type(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(path) if path.qself.is_none() => render_path(&path.path),
        syn::Type::Reference(reference) => render_reference(reference),
        syn::Type::Slice(slice) => Some(format!("[{}]", render_type(&slice.elem)?)),
        syn::Type::Tuple(tuple) if tuple.elems.is_empty() => Some("()".to_owned()),
        _ => None,
    }
}

fn render_reference(reference: &syn::TypeReference) -> Option<String> {
    let inner = render_type(&reference.elem)?;
    let mut text = String::from("&");
    if let Some(lifetime) = &reference.lifetime {
        text.push('\'');
        text.push_str(&lifetime.ident.to_string());
        text.push(' ');
    }
    if reference.mutability.is_some() {
        text.push_str("mut ");
    }
    text.push_str(&inner);
    Some(text)
}

fn render_path(path: &syn::Path) -> Option<String> {
    if path
        .segments
        .iter()
        .any(|segment| !matches!(segment.arguments, syn::PathArguments::None))
    {
        return None;
    }
    let parts = path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>();
    Some(parts.join("::"))
}
