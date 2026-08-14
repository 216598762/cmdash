use std::{collections::BTreeMap, fmt};

use ratatui::layout::Rect;

use crate::state::SessionId;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GraphicsResourceId {
    session: SessionId,
    image: u32,
}

impl GraphicsResourceId {
    pub const fn new(session: SessionId, image: u32) -> Self {
        Self { session, image }
    }

    pub const fn session(self) -> SessionId {
        self.session
    }

    pub const fn image(self) -> u32 {
        self.image
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphicsPlacement {
    resource: GraphicsResourceId,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    z_index: i16,
}

impl GraphicsPlacement {
    pub const fn resource(&self) -> GraphicsResourceId {
        self.resource
    }

    pub const fn x(&self) -> u16 {
        self.x
    }

    pub const fn y(&self) -> u16 {
        self.y
    }

    pub const fn width(&self) -> u16 {
        self.width
    }

    pub const fn height(&self) -> u16 {
        self.height
    }

    pub const fn z_index(&self) -> i16 {
        self.z_index
    }

    pub const fn area(&self) -> Rect {
        Rect::new(self.x, self.y, self.width, self.height)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphicsSubmission {
    resource: GraphicsResourceId,
    format: u8,
    encoded_payload: Vec<u8>,
    placement: GraphicsPlacement,
}

impl GraphicsSubmission {
    pub const fn resource(&self) -> GraphicsResourceId {
        self.resource
    }

    pub const fn format(&self) -> u8 {
        self.format
    }

    pub fn encoded_payload(&self) -> &[u8] {
        &self.encoded_payload
    }

    pub const fn placement(&self) -> &GraphicsPlacement {
        &self.placement
    }

    pub fn terminal_image_id(&self) -> u32 {
        terminal_image_id(self.resource)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphicsError {
    MissingAction,
    InvalidParameter(String),
    InvalidImageId,
    InvalidPayload,
}

impl fmt::Display for GraphicsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingAction => formatter.write_str("Kitty graphics command has no action"),
            Self::InvalidParameter(parameter) => {
                write!(formatter, "invalid Kitty graphics parameter {parameter:?}")
            }
            Self::InvalidImageId => formatter.write_str("Kitty graphics image id must be nonzero"),
            Self::InvalidPayload => formatter.write_str("invalid Kitty graphics base64 payload"),
        }
    }
}

impl std::error::Error for GraphicsError {}

#[derive(Clone, Debug)]
struct GraphicsResource {
    format: u8,
    decoded_payload: Vec<u8>,
    encoded_payload: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct SessionGraphicsStore {
    session: SessionId,
    resources: BTreeMap<u32, GraphicsResource>,
    placements: BTreeMap<u32, GraphicsPlacement>,
}

impl SessionGraphicsStore {
    pub const fn new(session: SessionId) -> Self {
        Self {
            session,
            resources: BTreeMap::new(),
            placements: BTreeMap::new(),
        }
    }

    pub const fn session(&self) -> SessionId {
        self.session
    }

    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    pub fn placement_count(&self) -> usize {
        self.placements.len()
    }

    pub fn decoded_bytes(&self, image: u32) -> Option<&[u8]> {
        self.resources
            .get(&image)
            .map(|resource| resource.decoded_payload.as_slice())
    }

    pub fn apply_kitty_command(
        &mut self,
        parameters: &[u8],
        encoded_payload: &[u8],
    ) -> Result<(), GraphicsError> {
        let values = parse_parameters(parameters)?;
        let action = values
            .get("a")
            .and_then(|value| value.as_bytes().first())
            .copied();
        let image = values
            .get("i")
            .map(|value| {
                value
                    .parse::<u32>()
                    .map_err(|_| GraphicsError::InvalidImageId)
            })
            .transpose()?
            .unwrap_or(0);

        match action {
            Some(b'T') | Some(b't') => {
                if image == 0 {
                    return Err(GraphicsError::InvalidImageId);
                }
                let decoded_payload =
                    decode_base64(encoded_payload).ok_or(GraphicsError::InvalidPayload)?;
                let format = values
                    .get("f")
                    .map(|value| {
                        value
                            .parse::<u8>()
                            .map_err(|_| GraphicsError::InvalidParameter(value.clone()))
                    })
                    .transpose()?
                    .unwrap_or(32);
                self.resources.insert(
                    image,
                    GraphicsResource {
                        format,
                        decoded_payload,
                        encoded_payload: encoded_payload.to_vec(),
                    },
                );
            }
            Some(b'p') | Some(b'P') => {
                let resource = self
                    .resources
                    .get(&image)
                    .ok_or(GraphicsError::InvalidImageId)?;
                let placement = GraphicsPlacement {
                    resource: GraphicsResourceId::new(self.session, image),
                    x: parameter_u16(&values, "x", 0)?,
                    y: parameter_u16(&values, "y", 0)?,
                    width: parameter_u16(&values, "c", 1)?.max(1),
                    height: parameter_u16(&values, "r", 1)?.max(1),
                    z_index: parameter_i16(&values, "z", 0)?,
                };
                let _ = resource;
                self.placements.insert(image, placement);
            }
            Some(b'd') | Some(b'D') => match values.get("d").map(String::as_str) {
                Some("a") => {
                    self.resources.clear();
                    self.placements.clear();
                }
                Some("p") => {
                    self.placements.clear();
                }
                _ if image != 0 => {
                    self.resources.remove(&image);
                    self.placements.remove(&image);
                }
                _ => return Err(GraphicsError::InvalidParameter("d".to_owned())),
            },
            Some(action) => {
                return Err(GraphicsError::InvalidParameter(
                    (action as char).to_string(),
                ));
            }
            None => return Err(GraphicsError::MissingAction),
        }
        Ok(())
    }

    pub fn visible_submissions(&self, surface: Rect) -> Vec<GraphicsSubmission> {
        let mut submissions = self
            .placements
            .values()
            .filter_map(|placement| {
                let resource = self.resources.get(&placement.resource.image())?;
                let placement_area = Rect::new(
                    surface.x.saturating_add(placement.x),
                    surface.y.saturating_add(placement.y),
                    placement.width,
                    placement.height,
                );
                let clipped_area = intersect(placement_area, surface)?;
                Some(GraphicsSubmission {
                    resource: placement.resource,
                    format: resource.format,
                    encoded_payload: resource.encoded_payload.clone(),
                    placement: GraphicsPlacement {
                        x: clipped_area.x,
                        y: clipped_area.y,
                        width: clipped_area.width,
                        height: clipped_area.height,
                        ..*placement
                    },
                })
            })
            .collect::<Vec<_>>();
        submissions.sort_by_key(|submission| submission.placement.z_index());
        submissions
    }
}

fn parse_parameters(parameters: &[u8]) -> Result<BTreeMap<String, String>, GraphicsError> {
    let mut values = BTreeMap::new();
    for parameter in parameters.split(|byte| *byte == b',') {
        if parameter.is_empty() {
            continue;
        }
        let separator = parameter
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or_else(|| {
                GraphicsError::InvalidParameter(String::from_utf8_lossy(parameter).into_owned())
            })?;
        let key = String::from_utf8_lossy(&parameter[..separator]).into_owned();
        let value = String::from_utf8_lossy(&parameter[separator + 1..]).into_owned();
        values.insert(key, value);
    }
    Ok(values)
}

fn parameter_u16(
    values: &BTreeMap<String, String>,
    key: &str,
    default: u16,
) -> Result<u16, GraphicsError> {
    values
        .get(key)
        .map(|value| {
            value
                .parse()
                .map_err(|_| GraphicsError::InvalidParameter(value.clone()))
        })
        .unwrap_or(Ok(default))
}

fn parameter_i16(
    values: &BTreeMap<String, String>,
    key: &str,
    default: i16,
) -> Result<i16, GraphicsError> {
    values
        .get(key)
        .map(|value| {
            value
                .parse()
                .map_err(|_| GraphicsError::InvalidParameter(value.clone()))
        })
        .unwrap_or(Ok(default))
}

pub fn terminal_image_id(resource: GraphicsResourceId) -> u32 {
    let session = resource.session().get() as u32;
    session.wrapping_mul(0x0010_0001) ^ resource.image()
}

fn decode_base64(payload: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    let mut accumulator = 0u32;
    let mut bits = 0u8;
    for byte in payload
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
    {
        if byte == b'=' {
            break;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
            accumulator &= (1 << bits) - 1;
        }
    }
    Some(output)
}

fn intersect(first: Rect, second: Rect) -> Option<Rect> {
    let left = (first.x as u32).max(second.x as u32);
    let top = (first.y as u32).max(second.y as u32);
    let right = (first.x as u32 + first.width as u32).min(second.x as u32 + second.width as u32);
    let bottom = (first.y as u32 + first.height as u32).min(second.y as u32 + second.height as u32);
    if left >= right || top >= bottom {
        return None;
    }
    Some(Rect::new(
        left as u16,
        top as u16,
        (right - left) as u16,
        (bottom - top) as u16,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resources_and_placements_are_namespaced_by_session() {
        let mut first = SessionGraphicsStore::new(SessionId::new(1));
        let mut second = SessionGraphicsStore::new(SessionId::new(2));
        first.apply_kitty_command(b"a=T,f=24,i=1", b"AQID").unwrap();
        first
            .apply_kitty_command(b"a=p,i=1,x=1,y=2,c=3,r=4", b"")
            .unwrap();
        second
            .apply_kitty_command(b"a=T,f=24,i=1", b"BAUG")
            .unwrap();
        second
            .apply_kitty_command(b"a=p,i=1,x=1,y=2,c=3,r=4", b"")
            .unwrap();

        let first_submission = first.visible_submissions(Rect::new(0, 0, 20, 10));
        let second_submission = second.visible_submissions(Rect::new(0, 0, 20, 10));
        assert_ne!(
            first_submission[0].resource(),
            second_submission[0].resource()
        );
        assert_ne!(
            first_submission[0].terminal_image_id(),
            second_submission[0].terminal_image_id()
        );
        assert_ne!(
            first_submission[0].encoded_payload(),
            second_submission[0].encoded_payload()
        );
        assert_eq!(first.decoded_bytes(1), Some(&[1, 2, 3][..]));
    }

    #[test]
    fn placements_restore_with_surface_translation_and_clipping() {
        let mut store = SessionGraphicsStore::new(SessionId::new(3));
        store
            .apply_kitty_command(b"a=T,f=100,i=7", b"AQID")
            .unwrap();
        store
            .apply_kitty_command(b"a=p,i=7,x=2,y=1,c=4,r=2", b"")
            .unwrap();

        let submissions = store.visible_submissions(Rect::new(10, 5, 8, 4));
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].placement().area(), Rect::new(12, 6, 4, 2));
        assert_eq!(submissions[0].format(), 100);
    }

    #[test]
    fn delete_commands_remove_resources_and_placements() {
        let mut store = SessionGraphicsStore::new(SessionId::new(4));
        store.apply_kitty_command(b"a=T,i=1", b"AQID").unwrap();
        store.apply_kitty_command(b"a=d,i=1", b"").unwrap();
        assert_eq!(store.resource_count(), 0);
        assert_eq!(store.placement_count(), 0);
    }
}
