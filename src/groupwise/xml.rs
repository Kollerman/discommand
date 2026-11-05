//! Just enough XML for GroupWise's answers: elements with their attributes
//! and text, namespace prefixes dropped.

use crate::system::login_pages::unescape;

#[derive(Debug, Default, Clone)]
pub struct Node {
    /// The local name (`gwt:subject` → `subject`).
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
    pub text: String,
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// The node at a path of child names ("distribution/from/email").
    pub fn at(&self, path: &str) -> Option<&Node> {
        path.split('/').try_fold(self, |n, part| n.child(part))
    }

    /// The text at a path, if there's any.
    pub fn text_at(&self, path: &str) -> Option<&str> {
        self.at(path).map(|n| n.text.trim()).filter(|t| !t.is_empty())
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The first node with this name anywhere below (depth first).
    pub fn find(&self, name: &str) -> Option<&Node> {
        self.children.iter().find_map(|c| if c.name == name { Some(c) } else { c.find(name) })
    }
}

fn local(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_string()
}

/// Parses a document into its root element.
pub fn parse(xml: &str) -> Option<Node> {
    let mut stack: Vec<Node> = vec![Node::default()];
    let mut rest = xml;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            stack.last_mut()?.text.push_str(&unescape(rest));
            break;
        };
        if lt > 0 {
            stack.last_mut()?.text.push_str(&unescape(&rest[..lt]));
        }
        rest = &rest[lt..];
        if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let end = r.find("]]>")?;
            stack.last_mut()?.text.push_str(&r[..end]);
            rest = &r[end + 3..];
            continue;
        }
        if rest.starts_with("<!--") {
            rest = &rest[rest.find("-->")? + 3..];
            continue;
        }
        if rest.starts_with("<?") || rest.starts_with("<!") {
            rest = &rest[rest.find('>')? + 1..];
            continue;
        }
        // The tag's end: the first `>` outside quotes.
        let mut quote = None;
        let end = rest.char_indices().skip(1).find_map(|(i, c)| {
            match (quote, c) {
                (None, '"' | '\'') => quote = Some(c),
                (Some(q), c) if c == q => quote = None,
                (None, '>') => return Some(i),
                _ => {}
            }
            None
        })?;
        let tag = &rest[1..end];
        rest = &rest[end + 1..];
        if let Some(name) = tag.strip_prefix('/') {
            let node = stack.pop()?;
            if local(name.trim()) != node.name || stack.is_empty() {
                return None;
            }
            stack.last_mut()?.children.push(node);
            continue;
        }
        let closed = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let name_end = tag.find(char::is_whitespace).unwrap_or(tag.len());
        let attrs = crate::system::login_pages::attributes(tag)
            .into_iter()
            .filter(|(k, _)| !k.starts_with("xmlns"))
            .map(|(k, v)| (local(&k), v))
            .collect();
        let node = Node { name: local(&tag[..name_end]), attrs, ..Node::default() };
        if closed {
            stack.last_mut()?.children.push(node);
        } else {
            stack.push(node);
        }
    }
    if stack.len() != 1 {
        return None;
    }
    stack.pop()?.children.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn reads_nested_answers() {
        let xml = r#"<?xml version="1.0"?><s:Envelope xmlns:s="x"><s:Body><gwm:getItemsResponse><gwm:items><gwt:item xsi:type="gwt:Mail"><gwt:id>A.1</gwt:id><gwt:subject>Hi &amp; bye</gwt:subject><gwt:distribution><gwt:from><gwt:displayName>Ann</gwt:displayName><gwt:email>ann@x.de</gwt:email></gwt:from></gwt:distribution><gwt:hasAttachment/></gwt:item><gwt:item><gwt:id>A.2</gwt:id></gwt:item></gwm:items></gwm:getItemsResponse></s:Body></s:Envelope>"#;
        let root = parse(xml).unwrap();
        let items = root.find("items").unwrap();
        let first = items.all("item").next().unwrap();
        assert_eq!(first.attr("type"), Some("gwt:Mail"));
        assert_eq!(first.text_at("subject"), Some("Hi & bye"));
        assert_eq!(first.text_at("distribution/from/email"), Some("ann@x.de"));
        assert!(first.child("hasAttachment").is_some());
        assert_eq!(items.all("item").count(), 2);
        assert!(parse("<a><b></a>").is_none());
    }
}
