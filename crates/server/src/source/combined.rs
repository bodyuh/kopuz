//! Spotify and SoundCloud as one source.
//!
//! The source the person picked stays the one everything is filed under; the
//! other service's signed-in source rides beside it. Each request goes to the
//! service it is about: a track by its service, an id by its shape (a
//! Spotify id is 22 base-62 characters, a SoundCloud one is a number).
//! Listings that cover the account (likes, playlists, the library) are both
//! services' together; search asks one, picked by its filter.

use async_trait::async_trait;
use config::{MusicService, Source};
use db::Db;

use super::{
    AuthOutcome, Capabilities, FavoritesPage, LibrarySnapshot, MediaSource, PlaylistMeta,
    RemoteAlbum, SearchFilterEntry, SourceError, StreamInfo,
};
use crate::ytmusic::browse::search::SearchPage;
use crate::ytmusic::discover::{DiscoverHome, DiscoverItem, DiscoverShelf, ShelfLayout, YtArtist};

/// Search filter ids, one per service.
const SPOTIFY: &str = "spotify";
const SOUNDCLOUD: &str = "soundcloud";

/// Favorites cursors say which service they page.
const FIRST: &str = "1:";
const SECOND: &str = "2:";

pub(super) struct CombinedSource {
    primary: Box<dyn MediaSource>,
    primary_service: MusicService,
    secondary: Box<dyn MediaSource>,
    secondary_service: MusicService,
}

impl CombinedSource {
    pub(super) fn new(
        primary: Box<dyn MediaSource>,
        primary_service: MusicService,
        secondary: Box<dyn MediaSource>,
        secondary_service: MusicService,
    ) -> Self {
        Self {
            primary,
            primary_service,
            secondary,
            secondary_service,
        }
    }

    fn of(&self, service: Option<MusicService>) -> &dyn MediaSource {
        if service == Some(self.secondary_service) {
            &*self.secondary
        } else {
            &*self.primary
        }
    }

    fn named(&self, filter: &str) -> &dyn MediaSource {
        self.of(match filter {
            SPOTIFY => Some(MusicService::Spotify),
            SOUNDCLOUD => Some(MusicService::SoundCloud),
            _ => None,
        })
    }

    /// The service an id belongs to, by its shape.
    fn by_id(&self, id: &str) -> &dyn MediaSource {
        let spotify = id.len() == 22
            && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
            && !id.bytes().all(|byte| byte.is_ascii_digit());
        let soundcloud = !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit());
        self.of(if spotify {
            Some(MusicService::Spotify)
        } else if soundcloud {
            Some(MusicService::SoundCloud)
        } else {
            None
        })
    }

    fn filter_of(service: MusicService) -> SearchFilterEntry {
        match service {
            MusicService::Spotify => SearchFilterEntry {
                id: SPOTIFY,
                label: "search_filter_spotify",
            },
            _ => SearchFilterEntry {
                id: SOUNDCLOUD,
                label: "search_filter_soundcloud",
            },
        }
    }
}

/// Both answers, or the one that came when the other failed.
fn either<T>(
    first: Result<Vec<T>, SourceError>,
    second: Result<Vec<T>, SourceError>,
) -> Result<Vec<T>, SourceError> {
    match (first, second) {
        (Ok(mut first), Ok(second)) => {
            first.extend(second);
            Ok(first)
        }
        (Ok(one), Err(error)) | (Err(error), Ok(one)) => {
            tracing::warn!(%error, "one service of the combined source failed");
            Ok(one)
        }
        (Err(error), Err(_)) => Err(error),
    }
}

#[async_trait]
impl MediaSource for CombinedSource {
    fn source(&self) -> &Source {
        self.primary.source()
    }

    fn db(&self) -> &Db {
        self.primary.db()
    }

    fn capabilities(&self) -> Capabilities {
        let mut caps = self.primary.capabilities();
        let other = self.secondary.capabilities();
        caps.external_devices |= other.external_devices;
        caps.browser_playback |= other.browser_playback;
        caps.discover |= other.discover;
        caps.sync |= other.sync;
        caps
    }

    async fn resolve_stream(
        &self,
        item_id: &str,
        quality: config::StreamQuality,
    ) -> Result<StreamInfo, SourceError> {
        self.by_id(item_id).resolve_stream(item_id, quality).await
    }

    async fn validate(&self) -> AuthOutcome {
        self.primary.validate().await
    }

    fn web_url(&self, track: &reader::Track) -> Option<String> {
        self.of(track.id.service()).web_url(track)
    }

    fn album_web_url(&self, browse_id: &str) -> Option<String> {
        self.by_id(browse_id).album_web_url(browse_id)
    }

    fn artist_web_url(&self, artist_id: &str) -> Option<String> {
        self.by_id(artist_id).artist_web_url(artist_id)
    }

    fn playlist_web_url(&self, playlist_id: &str) -> Option<String> {
        self.by_id(playlist_id).playlist_web_url(playlist_id)
    }

    async fn fetch_track(&self, item_id: &str) -> Result<Option<reader::Track>, SourceError> {
        self.by_id(item_id).fetch_track(item_id).await
    }

    async fn account_avatar(&self) -> Result<Option<String>, SourceError> {
        self.primary.account_avatar().await
    }

    async fn fetch_favorites(&self) -> Result<Vec<String>, SourceError> {
        either(
            self.primary.fetch_favorites().await,
            self.secondary.fetch_favorites().await,
        )
    }

    /// The first service's pages, then the second's.
    async fn fetch_favorites_page(
        &self,
        cursor: Option<String>,
    ) -> Result<FavoritesPage, SourceError> {
        let (second, inner) = match cursor.as_deref() {
            None => (false, None),
            Some(text) => match (text.strip_prefix(FIRST), text.strip_prefix(SECOND)) {
                (Some(rest), _) => (false, Some(rest.to_string())),
                (_, Some(rest)) => (true, (!rest.is_empty()).then(|| rest.to_string())),
                // A cursor from before the sources were joined.
                _ => (false, Some(text.to_string())),
            },
        };
        if second {
            let page = self.secondary.fetch_favorites_page(inner).await?;
            return Ok(FavoritesPage {
                tracks: page.tracks,
                next: page.next.map(|next| format!("{SECOND}{next}")),
            });
        }
        match self.primary.fetch_favorites_page(inner).await {
            Ok(page) => Ok(FavoritesPage {
                tracks: page.tracks,
                next: Some(match page.next {
                    Some(next) => format!("{FIRST}{next}"),
                    None => SECOND.to_string(),
                }),
            }),
            Err(error) => {
                tracing::warn!(%error, "first service's likes failed; paging the second");
                let page = self.secondary.fetch_favorites_page(None).await?;
                Ok(FavoritesPage {
                    tracks: page.tracks,
                    next: page.next.map(|next| format!("{SECOND}{next}")),
                })
            }
        }
    }

    async fn push_favorite(&self, item_id: &str, on: bool) -> Result<(), SourceError> {
        self.by_id(item_id).push_favorite(item_id, on).await
    }

    async fn search(
        &self,
        query: &str,
    ) -> Result<(Vec<reader::Track>, Vec<reader::Album>), SourceError> {
        let (first, second) = (
            self.primary.search(query).await,
            self.secondary.search(query).await,
        );
        match (first, second) {
            (Ok((mut tracks, mut albums)), Ok((more_tracks, more_albums))) => {
                tracks.extend(more_tracks);
                albums.extend(more_albums);
                Ok((tracks, albums))
            }
            (Ok(one), Err(error)) | (Err(error), Ok(one)) => {
                tracing::warn!(%error, "one service's search failed");
                Ok(one)
            }
            (Err(error), Err(_)) => Err(error),
        }
    }

    fn search_filters(&self) -> Vec<SearchFilterEntry> {
        vec![
            Self::filter_of(self.primary_service),
            Self::filter_of(self.secondary_service),
        ]
    }

    async fn search_shelves(
        &self,
        query: &str,
        filter: &str,
        continuation: Option<&str>,
    ) -> Result<SearchPage, SourceError> {
        let mut shelves = Vec::new();
        if continuation.is_some() {
            return Ok(SearchPage {
                shelves,
                continuation: None,
                correction: None,
            });
        }
        let source = self.named(filter);
        let shelf = |title: &str, items: Vec<DiscoverItem>, layout: ShelfLayout| DiscoverShelf {
            title: title.to_string(),
            strapline: None,
            more: None,
            items,
            layout,
            continuation: None,
            search_filter: None,
        };
        let artists = source.search_artists(query).await.unwrap_or_else(|error| {
            tracing::warn!(%error, "artist search failed");
            Vec::new()
        });
        let (tracks, albums) = source.search(query).await?;
        if !tracks.is_empty() {
            let songs = tracks
                .into_iter()
                .map(|track| DiscoverItem::Song(Box::new(track), Default::default()))
                .collect();
            shelves.push(shelf("Songs", songs, ShelfLayout::List));
        }
        if !artists.is_empty() {
            shelves.push(shelf("Artists", artists, ShelfLayout::Carousel));
        }
        if !albums.is_empty() {
            let albums = albums
                .into_iter()
                .map(|album| DiscoverItem::Album {
                    subtitle: album.artist.clone(),
                    title: album.title.clone(),
                    thumbnail: album
                        .cover_path
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                    browse_id: album.id.clone(),
                    actions: Default::default(),
                    explicit: false,
                })
                .collect();
            shelves.push(shelf("Albums", albums, ShelfLayout::Carousel));
        }
        Ok(SearchPage {
            shelves,
            continuation: None,
            correction: None,
        })
    }

    async fn search_artists(&self, query: &str) -> Result<Vec<DiscoverItem>, SourceError> {
        either(
            self.primary.search_artists(query).await,
            self.secondary.search_artists(query).await,
        )
    }

    async fn discover_home(&self) -> Result<DiscoverHome, SourceError> {
        let spotify = if self.primary_service == MusicService::Spotify {
            &*self.primary
        } else {
            &*self.secondary
        };
        spotify.discover_home().await
    }

    async fn fetch_artist(&self, channel_id: &str) -> Result<YtArtist, SourceError> {
        let first = self.by_id(channel_id);
        match first.fetch_artist(channel_id).await {
            Ok(artist) => Ok(artist),
            // A name, not an id: the one service that knows it answers.
            Err(error) => {
                let other = if std::ptr::addr_eq(first, &*self.primary) {
                    &*self.secondary
                } else {
                    &*self.primary
                };
                other.fetch_artist(channel_id).await.map_err(|_| error)
            }
        }
    }

    async fn fetch_album_tracks(&self, album_id: &str) -> Result<Vec<reader::Track>, SourceError> {
        self.by_id(album_id).fetch_album_tracks(album_id).await
    }

    async fn fetch_album_by_ref(&self, id: &str) -> Result<Option<RemoteAlbum>, SourceError> {
        self.by_id(id).fetch_album_by_ref(id).await
    }

    async fn fetch_library(&self) -> Result<LibrarySnapshot, SourceError> {
        let mut library = self.primary.fetch_library().await?;
        match self.secondary.fetch_library().await {
            Ok(more) => {
                library.albums.extend(more.albums);
                library.tracks.extend(more.tracks);
                library.artists.extend(more.artists);
            }
            Err(error) => tracing::warn!(%error, "second service's library failed"),
        }
        Ok(library)
    }

    async fn fetch_playlists(&self) -> Result<Vec<PlaylistMeta>, SourceError> {
        either(
            self.primary.fetch_playlists().await,
            self.secondary.fetch_playlists().await,
        )
    }

    async fn fetch_playlist_entries(
        &self,
        playlist_id: &str,
    ) -> Result<Vec<reader::Track>, SourceError> {
        self.by_id(playlist_id)
            .fetch_playlist_entries(playlist_id)
            .await
    }

    async fn add_to_playlist(
        &self,
        playlist_id: &str,
        item_refs: &[String],
    ) -> Result<Vec<String>, SourceError> {
        self.by_id(playlist_id)
            .add_to_playlist(playlist_id, item_refs)
            .await
    }

    async fn create_playlist(
        &self,
        name: &str,
        item_refs: &[String],
    ) -> Result<String, SourceError> {
        self.primary.create_playlist(name, item_refs).await
    }

    async fn remove_from_playlist(
        &self,
        playlist_id: &str,
        track: &reader::Track,
        position: usize,
    ) -> Result<(), SourceError> {
        self.by_id(playlist_id)
            .remove_from_playlist(playlist_id, track, position)
            .await
    }
}
