use anyhow::*;
use diesel;
use diesel::prelude::*;
#[cfg(feature = "profile-index")]
use flame;
use log::{error, info};
use regex::Regex;
use std::fs;
use std::path::Path;
use std::time;

use crate::config::MiscSettings;
use crate::db::{directories, misc_settings, songs, DB};
use crate::metadata;
use crate::vfs::VFSSource;

const INDEX_BUILDING_INSERT_BUFFER_SIZE: usize = 1000; // Insertions in each transaction
const INDEX_BUILDING_CLEAN_BUFFER_SIZE: usize = 500; // Insertions in each transaction

pub fn update(db: &DB) -> Result<()> {
	let start = time::Instant::now();
	info!("Beginning library index update");
	clean(db)?;
	populate(db)?;
	info!(
		"Library index update took {} seconds",
		start.elapsed().as_secs()
	);
	#[cfg(feature = "profile-index")]
	flame::dump_html(&mut fs::File::create("index-flame-graph.html").unwrap()).unwrap();
	Ok(())
}

#[derive(Debug, Insertable)]
#[table_name = "songs"]
struct NewSong {
	path: String,
	parent: String,
	track_number: Option<i32>,
	disc_number: Option<i32>,
	title: Option<String>,
	artist: Option<String>,
	album_artist: Option<String>,
	year: Option<i32>,
	album: Option<String>,
	artwork: Option<String>,
	duration: Option<i32>,
}

#[derive(Debug, Insertable)]
#[table_name = "directories"]
struct NewDirectory {
	path: String,
	parent: Option<String>,
	artist: Option<String>,
	year: Option<i32>,
	album: Option<String>,
	artwork: Option<String>,
	date_added: i32,
}

struct IndexBuilder {
	new_songs: Vec<NewSong>,
	new_directories: Vec<NewDirectory>,
	db: DB,
	album_art_pattern: Regex,
}

impl IndexBuilder {
	#[cfg_attr(feature = "profile-index", flame)]
	fn new(db: DB, album_art_pattern: Regex) -> Result<IndexBuilder> {
		let mut new_songs = Vec::new();
		let mut new_directories = Vec::new();
		new_songs.reserve_exact(INDEX_BUILDING_INSERT_BUFFER_SIZE);
		new_directories.reserve_exact(INDEX_BUILDING_INSERT_BUFFER_SIZE);
		Ok(IndexBuilder {
			new_songs,
			new_directories,
			db,
			album_art_pattern,
		})
	}

	#[cfg_attr(feature = "profile-index", flame)]
	fn flush_songs(&mut self) -> Result<()> {
		let connection = self.db.connect()?;
		diesel::insert_into(songs::table)
			.values(&self.new_songs)
			.execute(&*connection)?; // TODO https://github.com/diesel-rs/diesel/issues/1822
		self.new_songs.clear();
		Ok(())
	}

	#[cfg_attr(feature = "profile-index", flame)]
	fn flush_directories(&mut self) -> Result<()> {
		let connection = self.db.connect()?;
		diesel::insert_into(directories::table)
			.values(&self.new_directories)
			.execute(&*connection)?; // TODO https://github.com/diesel-rs/diesel/issues/1822
		self.new_directories.clear();
		Ok(())
	}

	#[cfg_attr(feature = "profile-index", flame)]
	fn push_song(&mut self, song: NewSong) -> Result<()> {
		if self.new_songs.len() >= self.new_songs.capacity() {
			self.flush_songs()?;
		}
		self.new_songs.push(song);
		Ok(())
	}

	#[cfg_attr(feature = "profile-index", flame)]
	fn push_directory(&mut self, directory: NewDirectory) -> Result<()> {
		if self.new_directories.len() >= self.new_directories.capacity() {
			self.flush_directories()?;
		}
		self.new_directories.push(directory);
		Ok(())
	}

	fn get_artwork(&self, dir: &Path) -> Result<Option<String>> {
		for file in fs::read_dir(dir)? {
			let file = file?;
			if let Some(name_string) = file.file_name().to_str() {
				if self.album_art_pattern.is_match(name_string) {
					return Ok(file.path().to_str().map(|p| p.to_owned()));
				}
			}
		}
		Ok(None)
	}

	#[cfg_attr(feature = "profile-index", flame)]
	fn populate_directory(&mut self, parent: Option<&Path>, path: &Path) -> Result<()> {
		// Find artwork
		let artwork = self.get_artwork(path).unwrap_or(None);

		// Extract path and parent path
		let parent_string = parent.and_then(|p| p.to_str()).map(|s| s.to_owned());
		let path_string = path.to_str().ok_or(anyhow!("Invalid directory path"))?;

		// Find date added
		let metadata = fs::metadata(path_string)?;
		let created = metadata
			.created()
			.or_else(|_| metadata.modified())?
			.duration_since(time::UNIX_EPOCH)?
			.as_secs() as i32;

		let mut directory_album = None;
		let mut directory_year = None;
		let mut directory_artist = None;
		let mut inconsistent_directory_album = false;
		let mut inconsistent_directory_year = false;
		let mut inconsistent_directory_artist = false;

		// Sub directories
		let mut sub_directories = Vec::new();

		// Insert content
		for file in fs::read_dir(path)? {
			#[cfg(feature = "profile-index")]
			let _guard = flame::start_guard("directory-entry");
			let file_path = match file {
				Ok(ref f) => f.path(),
				_ => {
					error!("File read error within {}", path_string);
					break;
				}
			};

			if file_path.is_dir() {
				sub_directories.push(file_path.to_path_buf());
				continue;
			}

			if let Some(file_path_string) = file_path.to_str() {
				if let Ok(tags) = metadata::read(file_path.as_path()) {
					if tags.year.is_some() {
						inconsistent_directory_year |=
							directory_year.is_some() && directory_year != tags.year;
						directory_year = tags.year;
					}

					if tags.album.is_some() {
						inconsistent_directory_album |=
							directory_album.is_some() && directory_album != tags.album;
						directory_album = tags.album.as_ref().cloned();
					}

					if tags.album_artist.is_some() {
						inconsistent_directory_artist |=
							directory_artist.is_some() && directory_artist != tags.album_artist;
						directory_artist = tags.album_artist.as_ref().cloned();
					} else if tags.artist.is_some() {
						inconsistent_directory_artist |=
							directory_artist.is_some() && directory_artist != tags.artist;
						directory_artist = tags.artist.as_ref().cloned();
					}

					let song = NewSong {
						path: file_path_string.to_owned(),
						parent: path_string.to_owned(),
						disc_number: tags.disc_number.map(|n| n as i32),
						track_number: tags.track_number.map(|n| n as i32),
						title: tags.title,
						duration: tags.duration.map(|n| n as i32),
						artist: tags.artist,
						album_artist: tags.album_artist,
						album: tags.album,
						year: tags.year,
						artwork: artwork.as_ref().cloned(),
					};

					self.push_song(song)?;
				}
			}
		}

		// Insert directory
		if inconsistent_directory_year {
			directory_year = None;
		}
		if inconsistent_directory_album {
			directory_album = None;
		}
		if inconsistent_directory_artist {
			directory_artist = None;
		}

		let directory = NewDirectory {
			path: path_string.to_owned(),
			parent: parent_string,
			artwork,
			album: directory_album,
			artist: directory_artist,
			year: directory_year,
			date_added: created,
		};
		self.push_directory(directory)?;

		// Populate subdirectories
		for sub_directory in sub_directories {
			self.populate_directory(Some(path), &sub_directory)?;
		}

		Ok(())
	}
}

#[cfg_attr(feature = "profile-index", flame)]
pub fn clean(db: &DB) -> Result<()> {
	let vfs = db.get_vfs()?;

	{
		let all_songs: Vec<String>;
		{
			let connection = db.connect()?;
			all_songs = songs::table.select(songs::path).load(&connection)?;
		}

		let missing_songs = all_songs
			.into_iter()
			.filter(|ref song_path| {
				let path = Path::new(&song_path);
				!path.exists() || vfs.real_to_virtual(path).is_err()
			})
			.collect::<Vec<_>>();

		{
			let connection = db.connect()?;
			for chunk in missing_songs[..].chunks(INDEX_BUILDING_CLEAN_BUFFER_SIZE) {
				diesel::delete(songs::table.filter(songs::path.eq_any(chunk)))
					.execute(&connection)?;
			}
		}
	}

	{
		let all_directories: Vec<String>;
		{
			let connection = db.connect()?;
			all_directories = directories::table
				.select(directories::path)
				.load(&connection)?;
		}

		let missing_directories = all_directories
			.into_iter()
			.filter(|ref directory_path| {
				let path = Path::new(&directory_path);
				!path.exists() || vfs.real_to_virtual(path).is_err()
			})
			.collect::<Vec<_>>();

		{
			let connection = db.connect()?;
			for chunk in missing_directories[..].chunks(INDEX_BUILDING_CLEAN_BUFFER_SIZE) {
				diesel::delete(directories::table.filter(directories::path.eq_any(chunk)))
					.execute(&connection)?;
			}
		}
	}

	Ok(())
}

#[cfg_attr(feature = "profile-index", flame)]
pub fn populate(db: &DB) -> Result<()> {
	let vfs = db.get_vfs()?;
	let mount_points = vfs.get_mount_points();

	let album_art_pattern;
	{
		let connection = db.connect()?;
		let settings: MiscSettings = misc_settings::table.get_result(&connection)?;
		album_art_pattern = Regex::new(&settings.index_album_art_pattern)?;
	}

	let mut builder = IndexBuilder::new(db.clone(), album_art_pattern)?;
	for target in mount_points.values() {
		builder.populate_directory(None, target.as_path())?;
	}
	builder.flush_songs()?;
	builder.flush_directories()?;
	Ok(())
}
