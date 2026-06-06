// src\0.rs
#![windows_subsystem = "windows"]
#![allow(warnings)]
#![feature(portable_simd)]
#![allow(non_camel_case_types)]
#![feature(string_from_utf8_lossy_owned)]
#![allow(static_mut_refs)]

use mimalloc::MiMalloc;
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use std::{
	collections::{VecDeque},
	ffi::{CStr, CString},
	fs::{File, read_to_string},
	io,
	mem::{size_of, take, transmute, zeroed},
	path::Path,
	ptr::{null, null_mut},
	slice::from_raw_parts,
	str::from_utf8,
	sync::{
		Arc, LazyLock, Mutex, OnceLock, RwLock,
		atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering},
		mpsc,
	},
	thread,
};

use symphonia::core::{
	audio::{Audio, GenericAudioBufferRef},
	codecs::{
		CodecParameters,
		audio::{AudioDecoder, AudioDecoderOptions},
	},
	common::Limit,
	formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType, probe::Hint},
	io::MediaSourceStream,
	meta::{MetadataOptions, RawValue, StandardTag, StandardVisualKey},
	units::{Duration, TimeBase, Timestamp},
};

use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

use rubato::{Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType, WindowFunction};

use gxhash::{HashMap, HashMapExt};

use ringbuf::{
	HeapRb,
	traits::{Consumer, Observer, Producer, Split},
};

use windows::{
	Foundation::{TimeSpan, TypedEventHandler},
	Media::{
		MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControls, SystemMediaTransportControlsButton,
		SystemMediaTransportControlsButtonPressedEventArgs, SystemMediaTransportControlsTimelineProperties,
	},
	Win32::{
		Foundation::{HWND, PROPERTYKEY},
		Media::Audio::{
			DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator,
			eConsole, eRender,
		},
		System::{
			Com::{CLSCTX_ALL, CoCreateInstance},
			WinRT::{ISystemMediaTransportControlsInterop, RO_INIT_MULTITHREADED, RoInitialize},
		},
		UI::Shell::{ITaskbarList3, TaskbarList},
	},
	core::{HSTRING, PCWSTR, Result as WinResult, implement},
};

/// 覆盖标准 `eprintln!` 宏，使日志输出到 UI 控件
/// 注意：此宏在编译时覆盖 std::eprintln!，所有 eprintln! 调用都会走这里
macro_rules! eprintln {
        ($($arg:tt)*) => {{
                log_enqueue(format!($($arg)*));
        }};
}

// src\TEM.rs
unsafe fn ui_set_now_playing2(li_id: usize, idx: usize, info: &SongInfo) {
	PostMessageW(UI_HWND, WM_UI_NOW_PLAYING, li_id, idx as i64);
	// 更新窗口标题
	let window_title = if info.author.is_empty() { info.title.clone() } else { format!("{} - {}", &info.title, &info.author) };
	let ws = to_wstring(&window_title);
	SetWindowTextW(UI_HWND, ws.as_ptr());

	if g_is_load_tray
	{
		tray_set_tooltip(&window_title);
	};

	ui_cover_on_track_change(info);
	// eprintln!("pl {:?}",  info);
}

// src\cmd.rs
// 获取播放线程命令队列
unsafe fn push_pl_cmd(cmd: PlayerCommand) {
	{
		let mut q = v_cmd_queue.write().unwrap();
		q.push(cmd);
	}

	is_pl_cmd.store(true, Ordering::SeqCst);

	if g_ev_pl_quit != 0
	{
		SetEvent(g_ev_pl_quit);
	}
}

/// 如果当前处于暂停状态，则恢复播放
/// 用于切歌、新播放列表等场景：用户预期这些操作应立即开始播放
unsafe fn resume_if_paused() {
	// WAIT_TIMEOUT(258) = g_ev_resume 未设置 = 已暂停
	let is_paused = WaitForSingleObject(g_ev_resume, 0) == 258;

	if g_ev_pl_quit != 0
	{
		SetEvent(g_ev_pl_quit);
	};

	if is_paused
	{
		SetEvent(g_ev_resume);

		// 独占模式下广播恢复消息（请求独占）
		if g_is_exclusive.load(Ordering::SeqCst)
		{
			PostMessageW(0xFFFF, 51000, 0, G_HWND);
		};
	};
}

fn take_player_commands() -> Option<Vec<PlayerCommand>> {
	if is_pl_cmd.load(Ordering::SeqCst)
	{
		let mut q = v_cmd_queue.write().unwrap();
		let cmds = take(&mut *q);
		is_pl_cmd.store(false, Ordering::SeqCst);
		if cmds.is_empty() { None } else { Some(cmds) }
	}
	else
	{
		None
	}
}

fn set_pending_track_by_path(li_id: usize, path: &str) {
	let mut m = m_pending_track_by_path.lock().unwrap();
	m.insert(li_id, normalize_path_key(path));
}

fn try_resolve_pending_track_index(li_id: usize, playlist: &[SongInfo]) -> Option<usize> {
	let target = {
		let mut m = m_pending_track_by_path.lock().unwrap();
		m.remove(&li_id)
	}?;

	if let Some(pos) = playlist
		.iter()
		.position(|s| normalize_path_key(&s.path) == target)
	{
		return Some(pos);
	}

	let mut m = m_pending_track_by_path.lock().unwrap();
	m.insert(li_id, target);
	None
}

unsafe fn set_active_playlist(id: usize, start_idx: usize) -> bool {
	// 确保列表已在内存池中（UI 可能刚从数据库加载并显示，但 pool 还没有）
	let li_len = if let Ok(pool) = m_pl_pool.read()
		&& let Some(li) = pool.get(&id)
	{
		li.len()
	}
	else
	{
		let songs = db_load_playlist_items(id);
		let len = songs.len();
		let mut pool = m_pl_pool.write().unwrap();
		pool.insert(id, songs);
		len
	};

	if li_len == 0
	{
		return false;
	}
	let play_mode = db_load_playlist_play_mode(id);
	g_pl_mode.store(play_mode, Ordering::SeqCst);
	g_li_id.store(id, Ordering::SeqCst);
	ui_sync_playlist_tabs(id);
	g_track.store(start_idx.min(li_len - 1), Ordering::SeqCst);
	g_to_pos_ms.store(0, Ordering::SeqCst);
	g_pl_is_changed.store(true, Ordering::SeqCst);
	g_to_next.store(false, Ordering::SeqCst);
	g_to_prev.store(false, Ordering::SeqCst);

	if g_ev_pl_quit != 0
	{
		SetEvent(g_ev_pl_quit);
	}
	if g_ev_li_chang != 0
	{
		SetEvent(g_ev_li_chang);
	}

	true
}

unsafe fn set_active_playlist_with_resume(id: usize, start_idx: usize, start_ms: u64) -> bool {
	// 确保列表已在内存池中（UI 可能刚从数据库加载并显示，但 pool 还没有）
	let li_len = if let Ok(pool) = m_pl_pool.read()
		&& let Some(li) = pool.get(&id)
	{
		li.len()
	}
	else
	{
		let songs = db_load_playlist_items(id);
		let len = songs.len();
		let mut pool = m_pl_pool.write().unwrap();
		pool.insert(id, songs);
		len
	};

	if li_len == 0
	{
		return false;
	}

	let play_mode = db_load_playlist_play_mode(id);
	g_pl_mode.store(play_mode, Ordering::SeqCst);
	g_li_id.store(id, Ordering::SeqCst);
	ui_sync_playlist_tabs(id);
	g_track.store(start_idx.min(li_len - 1), Ordering::SeqCst);
	g_to_pos_ms.store(start_ms.min(u32::MAX as u64) as u32, Ordering::SeqCst);
	g_pl_is_changed.store(true, Ordering::SeqCst);
	g_to_next.store(false, Ordering::SeqCst);
	g_to_prev.store(false, Ordering::SeqCst);

	if g_ev_pl_quit != 0
	{
		SetEvent(g_ev_pl_quit);
	}
	if g_ev_li_chang != 0
	{
		SetEvent(g_ev_li_chang);
	}

	true
}

// src\conf.rs
unsafe fn conf_init() {
	if let Ok(s) = read_to_string(r"D:\float\disk\history\fog.txt")
	{
		for n in s.trim_start_matches('﻿').split('\n')
		{
			let r = n.trim();

			if r.is_empty()
			{
				continue;
			}

			if let Some((k, v)) = r.split_once('=')
			{
				let r = v.trim();

				if !r.is_empty()
				{
					match k.trim()
					{
						"root_dir" =>
						{
							g_root_dir = r.to_string();
						}
						"ffm_dir" =>
						{
							g_ffm_dir = r.to_string();
						}
						"font_size" =>
						{
							if let Ok(fs) = r.parse::<i32>()
							{
								g_font_size = fs;
							};
						}
						"load_tray" =>
						{
							if let Ok(lt) = r.parse::<i32>()
							{
								g_is_load_tray = lt != 0;
							};
						}
						"def_exclusive" =>
						{
							if let Ok(lt) = r.parse::<i32>()
							{
								g_def_exclusive = lt != 0;
							};
						}
						"pl_db_path" =>
						{
							g_pl_db_path = r.to_owned();
						}
						"serve_port" =>
						{
							if let Ok(sp) = r.parse::<u16>()
							{};
						}
						_ =>
						{}
					};
				};
			}
		}
	};
}

// src\db.rs
unsafe fn music_db_collect_scan_roots() -> Vec<(String, String)> {
	let mut roots: Vec<(String, String)> = Vec::new();
	let mut keys: Vec<String> = Vec::new();

	for root in g_root_dir.split('|')
	{
		let fixed = fix_scan_root(root);

		if fixed.is_empty()
		{
			continue;
		}

		let key = normalize_path_key(&fixed);
		if keys.iter().any(|k| k == &key)
		{
			continue;
		}
		keys.push(key.clone());
		roots.push((fixed, key));
	}

	roots
}

unsafe fn music_db_remove_root_records(root_id: i64, path: &str) {
	let base = if path.ends_with('\\') && path.len() > 3 { &path[..path.len() - 1] } else { path };
	let mut prefix = String::from(base);
	if !prefix.ends_with('\\')
	{
		prefix.push('\\');
	}
	let base_norm = normalize_path_key(base);
	let prefix_norm = normalize_path_key(&prefix);

	let sql = b"DELETE FROM songs WHERE root_id = ? OR (ifnull(root_id, 0) = 0 AND (lower(dir) = ? OR instr(lower(dir), ?) = 1));\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB_MUSIC, sql.as_ptr(), sql.len(), &mut stmt, 0) == SQLITE_OK
	{
		sqlite3_bind_int64(stmt, 1, root_id);
		sqlite3_bind_text(stmt, 2, base_norm.as_ptr(), base_norm.len(), -1);
		sqlite3_bind_text(stmt, 3, prefix_norm.as_ptr(), prefix_norm.len(), -1);
		sqlite3_step(stmt);
		sqlite3_finalize(stmt);
	}

	let sql_root = b"DELETE FROM root WHERE id=?;\0";
	if sqlite3_prepare_v2(HDB_MUSIC, sql_root.as_ptr(), sql_root.len(), &mut stmt, 0) == SQLITE_OK
	{
		sqlite3_bind_int64(stmt, 1, root_id);
		sqlite3_step(stmt);
		sqlite3_finalize(stmt);
	}
}

unsafe fn music_db_fixup_root_ids(roots: &[MusicRootInfo]) {
	for root in roots
	{
		if root.id <= 0
		{
			continue;
		}

		let base = if root.path.ends_with('\\') && root.path.len() > 3 { &root.path[..root.path.len() - 1] } else { root.path.as_str() };
		let mut prefix = String::from(base);
		if !prefix.ends_with('\\')
		{
			prefix.push('\\');
		}
		let base_norm = normalize_path_key(base);
		let prefix_norm = normalize_path_key(&prefix);

		let sql = b"UPDATE songs SET root_id=? WHERE ifnull(root_id, 0) = 0 AND (lower(dir) = ? OR instr(lower(dir), ?) = 1);\0";
		let mut stmt: i64 = 0;
		if sqlite3_prepare_v2(HDB_MUSIC, sql.as_ptr(), sql.len(), &mut stmt, 0) == SQLITE_OK
		{
			sqlite3_bind_int64(stmt, 1, root.id);
			sqlite3_bind_text(stmt, 2, base_norm.as_ptr(), base_norm.len(), -1);
			sqlite3_bind_text(stmt, 3, prefix_norm.as_ptr(), prefix_norm.len(), -1);
			sqlite3_step(stmt);
			sqlite3_finalize(stmt);
		}
	}

	sqlite3_exec(HDB_MUSIC, b"DELETE FROM songs WHERE ifnull(root_id, 0) = 0;\0".as_ptr(), None, null_mut(), null_mut());
}

unsafe fn music_db_sync_roots() -> Vec<MusicRootInfo> {
	let scan_roots = music_db_collect_scan_roots();
	let scan_keys: Vec<String> = scan_roots
		.iter()
		.map(|(_, key)| key.clone())
		.collect();

	let mut db_roots: Vec<(i64, String, String)> = Vec::new();
	let sql = b"SELECT id, path FROM root;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB_MUSIC, sql.as_ptr(), sql.len(), &mut stmt, 0) == SQLITE_OK
	{
		while sqlite3_step(stmt) == SQLITE_ROW
		{
			let id = sqlite3_column_int64(stmt, 0);
			let raw_path = sqlite_column_string_raw(stmt, 1);
			let path = fix_scan_root(&raw_path);
			if path.is_empty()
			{
				continue;
			}
			let key = normalize_path_key(&path);
			db_roots.push((id, path, key));
		}
		sqlite3_finalize(stmt);
	}

	let mut seen_keys: Vec<String> = Vec::new();
	let mut roots_to_remove: Vec<(i64, String)> = Vec::new();
	for (id, path, key) in db_roots.iter()
	{
		let in_scan = scan_keys.iter().any(|k| k == key);
		let dup = seen_keys.iter().any(|k| k == key);
		if in_scan && !dup
		{
			seen_keys.push(key.clone());
		}
		else
		{
			roots_to_remove.push((*id, path.clone()));
		}
	}

	for (id, path) in roots_to_remove.iter()
	{
		music_db_remove_root_records(*id, path);
	}

	let mut out: Vec<MusicRootInfo> = Vec::new();
	for (path, key) in scan_roots
	{
		if let Some((id, stored_path, _)) = db_roots.iter().find(|(id, _, k)| {
			!roots_to_remove
				.iter()
				.any(|(rid, _)| rid == id)
				&& k == &key
		})
		{
			if stored_path != &path
			{
				let sql = b"UPDATE root SET path=? WHERE id=?;\0";
				let mut stmt: i64 = 0;
				if sqlite3_prepare_v2(HDB_MUSIC, sql.as_ptr(), sql.len(), &mut stmt, 0) == SQLITE_OK
				{
					sqlite3_bind_text(stmt, 1, path.as_ptr(), path.len(), -1);
					sqlite3_bind_int64(stmt, 2, *id);
					sqlite3_step(stmt);
					sqlite3_finalize(stmt);
				}
			}
			out.push(MusicRootInfo { id: *id, path });
		}
		else
		{
			let sql = b"INSERT INTO root (path) VALUES (?);\0";
			let mut stmt: i64 = 0;
			let mut new_id = 0;
			if sqlite3_prepare_v2(HDB_MUSIC, sql.as_ptr(), sql.len(), &mut stmt, 0) == SQLITE_OK
			{
				sqlite3_bind_text(stmt, 1, path.as_ptr(), path.len(), -1);
				if sqlite3_step(stmt) == SQLITE_DONE
				{
					new_id = sqlite3_last_insert_rowid(HDB_MUSIC);
				}
				sqlite3_finalize(stmt);
			}
			out.push(MusicRootInfo { id: new_id, path });
		}
	}

	music_db_fixup_root_ids(&out);
	out
}

unsafe fn music_db_collect_song_path_map() -> HashMap<String, (String, String)> {
	let mut map: HashMap<String, (String, String)> = HashMap::default();

	let sql = b"SELECT dir, name FROM songs;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB_MUSIC, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return map;
	}

	while sqlite3_step(stmt) == SQLITE_ROW
	{
		let dir = sqlite_column_string_raw(stmt, 0);
		let name = sqlite_column_string_raw(stmt, 1);
		if dir.is_empty() || name.is_empty()
		{
			continue;
		}
		let mut full = String::with_capacity(dir.len() + name.len() + 1);
		full.push_str(&dir);
		full.push('\\');
		full.push_str(&name);
		let key = normalize_path_key(&full);
		map.entry(key).or_insert((dir, name));
	}
	sqlite3_finalize(stmt);

	map
}

unsafe fn music_db_cleanup_stale_paths(stale: HashMap<String, (String, String)>) {
	if stale.is_empty()
	{
		return;
	}

	sqlite3_exec(HDB_MUSIC, b"BEGIN TRANSACTION;\0".as_ptr(), None, null_mut(), null_mut());

	let sql = b"DELETE FROM songs WHERE dir = ? AND name = ?;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB_MUSIC, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		sqlite3_exec(HDB_MUSIC, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
		return;
	}

	let mut removed: u64 = 0;
	for (_key, (dir, name)) in stale
	{
		sqlite3_bind_text(stmt, 1, dir.as_ptr(), dir.len(), -1);
		sqlite3_bind_text(stmt, 2, name.as_ptr(), name.len(), -1);
		if sqlite3_step(stmt) == SQLITE_DONE
		{
			removed = removed.saturating_add(sqlite3_changes(HDB_MUSIC) as u64);
		}
		sqlite3_reset(stmt);
	}
	sqlite3_finalize(stmt);

	sqlite3_exec(HDB_MUSIC, b"COMMIT;\0".as_ptr(), None, null_mut(), null_mut());

	if removed > 0
	{
		eprintln!("[db] 清理无效记录: {}", removed);
	}
}

unsafe fn music_db_query_default_playlist(hdb: i64) -> Vec<SongInfo> {
	let sql =
		b"SELECT dir, name, size, title, artist, album, duration_ms, duration_text, codec, has_cover FROM songs ORDER BY dir, name;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(hdb, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return Vec::new();
	}

	let mut songs: Vec<SongInfo> = Vec::new();
	while sqlite3_step(stmt) == SQLITE_ROW
	{
		let dir = sqlite_column_string_raw(stmt, 0);
		let name = sqlite_column_string_raw(stmt, 1);
		if dir.is_empty() || name.is_empty()
		{
			continue;
		}

		let mut path = String::with_capacity(dir.len() + name.len() + 1);
		path.push_str(&dir);
		path.push('\\');
		path.push_str(&name);

		let mut song = SongInfo { path, ..Default::default() };
		song.file_size = sqlite3_column_int64(stmt, 2).max(0) as u64;
		song.title = sqlite_column_string(stmt, 3);
		song.author = sqlite_column_string(stmt, 4);
		song.album = sqlite_column_string(stmt, 5);
		if song.album.is_empty()
		{
			if let Some(name) = album_name_from_parent_dir(song.path.as_str())
			{
				song.album = name;
			}
		}
		song.duration_ms = sqlite3_column_int64(stmt, 6) as u64;
		song.duration_text = sqlite_column_string(stmt, 7);
		if song.duration_text.is_empty() && song.duration_ms > 0
		{
			song.duration_text = format_time(song.duration_ms);
		}
		song.codec = sqlite_column_string(stmt, 8);
		song.has_cover = sqlite3_column_int64(stmt, 9) != 0;

		if song.album_artist.is_empty() && !song.author.is_empty()
		{
			song.album_artist = song.author.clone();
		}

		songs.push(song);
	}
	sqlite3_finalize(stmt);
	songs
}

unsafe fn music_db_ensure_tables_for_query(hdb: i64) {
	let sql_root = b"CREATE TABLE IF NOT EXISTS root (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT UNIQUE
        );\0";
	sqlite3_exec(hdb, sql_root.as_ptr(), None, null_mut(), null_mut());

	let sql_songs = b"CREATE TABLE IF NOT EXISTS songs (
                ID INTEGER PRIMARY KEY AUTOINCREMENT,
                root_id INTEGER DEFAULT 0,
                dir TEXT,
                name TEXT,
                modify_time INTEGER,
                size INTEGER,
                title TEXT,
                artist TEXT,
                album TEXT,
                duration_ms INTEGER,
                duration_text TEXT,
                codec TEXT,
                has_cover INTEGER DEFAULT 0,
                UNIQUE(dir, name)
        );\0";
	sqlite3_exec(hdb, sql_songs.as_ptr(), None, null_mut(), null_mut());
}

unsafe fn music_db_open_for_query() -> Option<i64> {
	if MUSIC_DB_PATH.is_empty()
	{
		return None;
	}

	let mut hdb: i64 = 0;
	if sqlite3_open16(to_wstring(MUSIC_DB_PATH).as_ptr(), &mut hdb) != SQLITE_OK || hdb == 0
	{
		return None;
	}

	sqlite3_exec(hdb, b"PRAGMA busy_timeout=200;\0".as_ptr(), None, null_mut(), null_mut());
	music_db_ensure_tables_for_query(hdb);
	Some(hdb)
}

unsafe fn music_db_query_songs_by_dir_prefix(hdb: i64, dir_prefix: &str) -> Vec<SongInfo> {
	let base = dir_prefix.trim();
	if base.is_empty()
	{
		return Vec::new();
	}
	let base = if base.ends_with('\\') && base.len() > 3 { &base[..base.len() - 1] } else { base };

	let mut prefix = String::from(base);
	if !prefix.ends_with('\\')
	{
		prefix.push('\\');
	}

	let base_norm = normalize_path_key(base);
	let prefix_norm = normalize_path_key(&prefix);

	let sql = b"SELECT dir, name, size, title, artist, album, duration_ms, duration_text, codec, has_cover FROM songs WHERE lower(dir) = ? OR instr(lower(dir), ?) = 1 ORDER BY dir, name;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(hdb, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return Vec::new();
	}
	sqlite3_bind_text(stmt, 1, base_norm.as_ptr(), base_norm.len(), -1);
	sqlite3_bind_text(stmt, 2, prefix_norm.as_ptr(), prefix_norm.len(), -1);

	let mut songs: Vec<SongInfo> = Vec::new();
	while sqlite3_step(stmt) == SQLITE_ROW
	{
		let dir = sqlite_column_string_raw(stmt, 0);
		let name = sqlite_column_string_raw(stmt, 1);
		if dir.is_empty() || name.is_empty()
		{
			continue;
		}

		let mut path = String::with_capacity(dir.len() + name.len() + 1);
		path.push_str(&dir);
		path.push('\\');
		path.push_str(&name);

		let mut song = SongInfo { path, ..Default::default() };
		song.file_size = sqlite3_column_int64(stmt, 2).max(0) as u64;
		song.title = sqlite_column_string(stmt, 3);
		song.author = sqlite_column_string(stmt, 4);
		song.album = sqlite_column_string(stmt, 5);
		if song.album.is_empty()
		{
			if let Some(name) = album_name_from_parent_dir(song.path.as_str())
			{
				song.album = name;
			}
		}
		song.duration_ms = sqlite3_column_int64(stmt, 6) as u64;
		song.duration_text = sqlite_column_string(stmt, 7);
		if song.duration_text.is_empty() && song.duration_ms > 0
		{
			song.duration_text = format_time(song.duration_ms);
		}
		song.codec = sqlite_column_string(stmt, 8);
		song.has_cover = sqlite3_column_int64(stmt, 9) != 0;

		if song.album_artist.is_empty() && !song.author.is_empty()
		{
			song.album_artist = song.author.clone();
		}

		songs.push(song);
	}
	sqlite3_finalize(stmt);
	songs
}

unsafe fn music_db_query_song_by_dir_name(hdb: i64, dir: &str, name: &str) -> Option<SongInfo> {
	let dir = dir.trim();
	let name = name.trim();
	if dir.is_empty() || name.is_empty()
	{
		return None;
	}

	let dir_norm = normalize_path_key(dir);
	let name_norm = name.to_ascii_lowercase();

	let sql = b"SELECT dir, name, size, title, artist, album, duration_ms, duration_text, codec, has_cover FROM songs WHERE lower(dir) = ? AND lower(name) = ? LIMIT 1;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(hdb, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return None;
	}
	sqlite3_bind_text(stmt, 1, dir_norm.as_ptr(), dir_norm.len(), -1);
	sqlite3_bind_text(stmt, 2, name_norm.as_ptr(), name_norm.len(), -1);

	let mut song: Option<SongInfo> = None;
	if sqlite3_step(stmt) == SQLITE_ROW
	{
		let dir = sqlite_column_string_raw(stmt, 0);
		let name = sqlite_column_string_raw(stmt, 1);
		if !dir.is_empty() && !name.is_empty()
		{
			let mut path = String::with_capacity(dir.len() + name.len() + 1);
			path.push_str(&dir);
			path.push('\\');
			path.push_str(&name);

			let mut s = SongInfo { path, ..Default::default() };
			s.file_size = sqlite3_column_int64(stmt, 2).max(0) as u64;
			s.title = sqlite_column_string(stmt, 3);
			s.author = sqlite_column_string(stmt, 4);
			s.album = sqlite_column_string(stmt, 5);
			if s.album.is_empty()
			{
				if let Some(name) = album_name_from_parent_dir(s.path.as_str())
				{
					s.album = name;
				}
			}
			s.duration_ms = sqlite3_column_int64(stmt, 6) as u64;
			s.duration_text = sqlite_column_string(stmt, 7);
			if s.duration_text.is_empty() && s.duration_ms > 0
			{
				s.duration_text = format_time(s.duration_ms);
			}
			s.codec = sqlite_column_string(stmt, 8);
			s.has_cover = sqlite3_column_int64(stmt, 9) != 0;
			if s.album_artist.is_empty() && !s.author.is_empty()
			{
				s.album_artist = s.author.clone();
			}
			song = Some(s);
		}
	}
	sqlite3_finalize(stmt);
	song
}

unsafe fn music_db_load_default_playlist_from_file() -> Vec<SongInfo> {
	if MUSIC_DB_PATH.is_empty()
	{
		return Vec::new();
	}

	let mut hdb: i64 = 0;
	if sqlite3_open16(to_wstring(MUSIC_DB_PATH).as_ptr(), &mut hdb) != SQLITE_OK || hdb == 0
	{
		return Vec::new();
	}

	// 允许与扫描线程并发：若被写锁占用，短暂等待。
	sqlite3_exec(hdb, b"PRAGMA busy_timeout=200;\0".as_ptr(), None, null_mut(), null_mut());

	// 兼容：如果 music.db 是新文件，确保 songs 表存在（否则查询会失败）
	let sql_root = b"CREATE TABLE IF NOT EXISTS root (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT UNIQUE
        );\0";
	sqlite3_exec(hdb, sql_root.as_ptr(), None, null_mut(), null_mut());

	let sql_songs = b"CREATE TABLE IF NOT EXISTS songs (
                ID INTEGER PRIMARY KEY AUTOINCREMENT,
                root_id INTEGER DEFAULT 0,
                dir TEXT,
                name TEXT,
                modify_time INTEGER,
                size INTEGER,
                title TEXT,
                artist TEXT,
                album TEXT,
                duration_ms INTEGER,
                duration_text TEXT,
                codec TEXT,
                has_cover INTEGER DEFAULT 0,
                UNIQUE(dir, name)
        );\0";
	sqlite3_exec(hdb, sql_songs.as_ptr(), None, null_mut(), null_mut());

	let songs = music_db_query_default_playlist(hdb);
	sqlite3_close(hdb);
	songs
}

unsafe fn music_db_refresh_default_playlist() {
	let songs = music_db_load_default_playlist_from_file();

	// 更新 fog.db 中的“默认”播放列表（id=1），让其成为普通持久化播放列表
	if !db_replace_playlist(PLAYLIST_ID_DEFAULT, Some(PLAYLIST_NAME_DEFAULT), &songs)
	{
		eprintln!("[db] 保存默认播放列表失败: id={}", PLAYLIST_ID_DEFAULT);
	}
	{
		let mut pool = m_pl_pool.write().unwrap();
		pool.insert(PLAYLIST_ID_DEFAULT, songs.clone());
	}
	ui_playlist_update(PLAYLIST_ID_DEFAULT, songs);
}

/// 扫描监控线程入口
unsafe fn music_scan_watch_thread() {
	music_db_init();

	if HDB_MUSIC == 0
	{
		return;
	}

	let scan_roots = music_db_sync_roots();
	let track_stale_paths = !scan_roots.is_empty();
	let mut stale_paths = if track_stale_paths { music_db_collect_song_path_map() } else { HashMap::default() };

	for root in scan_roots.iter()
	{
		eprintln!("[db] init root: {}", root.path);
		if track_stale_paths
		{
			music_db_scan_directory(&root.path, root.id, Some(&mut stale_paths));
		}
		else
		{
			music_db_scan_directory(&root.path, root.id, None);
		}
	}

	if track_stale_paths
	{
		music_db_cleanup_stale_paths(stale_paths);
	}

	ui_tree_refresh();
	music_db_refresh_default_playlist();

	if !scan_roots.is_empty()
	{
		watch_db_dir(&scan_roots);
	}
}

/// 递归遍历目录并获取所有文件路径 (拆分为 dir 和 name)，同时收集文件标志（修改日期、大小）
unsafe fn get_music_dir_li(dir_pattern: &str, li: &mut Vec<(String, String, u64, u64)>, p: &mut WIN32_FIND_DATAW) {
	let h = FindFirstFileW(to_wstring(dir_pattern).as_ptr(), p);

	if h == -1
	{
		return;
	}

	let from = &dir_pattern[..dir_pattern.len() - 1]; // 去掉末尾的 '*'
	// 处理目录前缀，去掉末尾的反斜杠以便存入 dir 列
	let dir_db = if from.ends_with('\\') && from.len() > 3 { &from[..from.len() - 1] } else { from };

	loop
	{
		if (p.dwFileAttributes & 16) == 0
		{
			// 是文件
			let name = String::from_utf16_lossy(&p.cFileName[..get_dir_u16(&p.cFileName)]);
			if let Some((_, ext)) = name.rsplit_once('.')
			{
				let ext = ext.to_lowercase();
				if is_supported_media_ext(&ext)
				{
					let mtime = filetime_to_u64(&p.ftLastWriteTime);
					let size = filesize_to_u64(p.nFileSizeHigh, p.nFileSizeLow);

					li.push((dir_db.to_string(), name, mtime, size));
				}
			}
		}
		else
		{
			// 是目录，跳过特殊目录 . 和 ..
			if !(p.cFileName[0] == 46 && ((p.cFileName[1] == 0) || (p.cFileName[1] == 46 && p.cFileName[2] == 0)))
			{
				let n = String::from_utf16_lossy(&p.cFileName[..get_dir_u16(&p.cFileName)]);
				let mut next_pattern = String::with_capacity(from.len() + n.len() + 2);

				next_pattern.push_str(from);
				next_pattern.push_str(n.as_str());
				next_pattern.push('\\');
				next_pattern.push('*');

				get_music_dir_li(&next_pattern, li, p);
			}
		}

		if 0 == FindNextFileW(h, p)
		{
			break;
		}
	}

	FindClose(h);
}

/// 从数据库获取现有文件的标志
unsafe fn music_db_get_file_flags(dir: &str, name: &str) -> Option<(u64, u64, i64)> {
	let sql = b"SELECT modify_time, size, root_id FROM songs WHERE dir = ? AND name = ?;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB_MUSIC, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return None;
	}

	sqlite3_bind_text(stmt, 1, dir.as_ptr(), dir.len(), -1);
	sqlite3_bind_text(stmt, 2, name.as_ptr(), name.len(), -1);

	let res = if sqlite3_step(stmt) == SQLITE_ROW
	{
		let mtime = sqlite3_column_int64(stmt, 0) as u64;
		let size = sqlite3_column_int64(stmt, 1) as u64;
		let root_id = sqlite3_column_int64(stmt, 2);
		Some((mtime, size, root_id))
	}
	else
	{
		None
	};

	sqlite3_finalize(stmt);
	res
}

unsafe fn music_db_update_song_root(root_id: i64, dir: &str, name: &str) {
	let sql = b"UPDATE songs SET root_id=? WHERE dir=? AND name=?;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB_MUSIC, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return;
	}

	sqlite3_bind_int64(stmt, 1, root_id);
	sqlite3_bind_text(stmt, 2, dir.as_ptr(), dir.len(), -1);
	sqlite3_bind_text(stmt, 3, name.as_ptr(), name.len(), -1);
	sqlite3_step(stmt);
	sqlite3_finalize(stmt);
}

/// 插入或更新歌曲信息
unsafe fn music_db_upsert_song(root_id: i64, dir: &str, name: &str, mtime: u64, size: u64, has_cover: bool, info: &FFmpegProbeMediaInfo) {
	let sql =
                b"INSERT OR REPLACE INTO songs (root_id, dir, name, modify_time, size, title, artist, album, duration_ms, duration_text, codec, has_cover) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB_MUSIC, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return;
	}

	let mut title = String::new();
	let mut artist = String::new();
	let mut album = String::new();

	for (k, v) in &info.tags
	{
		match k.to_lowercase().as_str()
		{
			"title" => title = v.clone(),
			"artist" | "author" => artist = v.clone(),
			"album" => album = v.clone(),
			_ =>
			{}
		}
	}

	// 如果没有标签，从文件名获取标题
	if title.is_empty()
	{
		if let Some((base, _)) = name.rsplit_once('.')
		{
			title = base.to_string();
		}
		else
		{
			title = name.to_string();
		}
	}

	if album.is_empty()
	{
		if let Some(name) = Path::new(dir)
			.file_name()
			.map(|v| v.to_string_lossy())
		{
			let name = name.trim();
			if !name.is_empty()
			{
				album = name.to_string();
			}
		}
	}

	sqlite3_bind_int64(stmt, 1, root_id);
	sqlite3_bind_text(stmt, 2, dir.as_ptr(), dir.len(), -1);
	sqlite3_bind_text(stmt, 3, name.as_ptr(), name.len(), -1);
	sqlite3_bind_int64(stmt, 4, mtime as i64);
	sqlite3_bind_int64(stmt, 5, size as i64);
	sqlite3_bind_text(stmt, 6, title.as_ptr(), title.len(), -1);
	sqlite3_bind_text(stmt, 7, artist.as_ptr(), artist.len(), -1);
	sqlite3_bind_text(stmt, 8, album.as_ptr(), album.len(), -1);
	sqlite3_bind_int64(stmt, 9, info.duration_ms as i64);
	let duration_text = if info.duration_ms > 0 { format_time(info.duration_ms) } else { String::new() };
	sqlite3_bind_text(stmt, 10, duration_text.as_ptr(), duration_text.len(), -1);
	sqlite3_bind_text(stmt, 11, info.codec_name.as_ptr(), info.codec_name.len(), -1);
	sqlite3_bind_int(stmt, 12, if has_cover { 1 } else { 0 });

	sqlite3_step(stmt);
	sqlite3_finalize(stmt);
}

/// 扫描目录入口 (递归扫描)
unsafe fn music_db_scan_directory(path: &str, root_id: i64, mut stale: Option<&mut HashMap<String, (String, String)>>) {
	eprintln!("[db] 扫描目录: {}", path);
	let mut li: Vec<(String, String, u64, u64)> = Vec::new();
	let mut p: WIN32_FIND_DATAW = zeroed();

	let pattern = if path.ends_with('\\') { format!("{}*", path) } else { format!("{}\\{}", path, "*") };
	get_music_dir_li(&pattern, &mut li, &mut p);

	let total = li.len();
	if total == 0
	{
		return;
	}

	sqlite3_exec(HDB_MUSIC, b"BEGIN TRANSACTION;\0".as_ptr(), None, null_mut(), null_mut());
	let mut count = 0;
	let mut skip = 0;

	for (dir, name, mtime, size) in li
	{
		let mut full_path = String::with_capacity(dir.len() + name.len() + 1);
		full_path.push_str(&dir);
		full_path.push('\\');
		full_path.push_str(&name);

		if let Some(stale_map) = stale.as_deref_mut()
		{
			let key = normalize_path_key(&full_path);
			stale_map.remove(&key);
		}

		if let Some((db_mtime, db_size, db_root_id)) = music_db_get_file_flags(&dir, &name)
		{
			if db_mtime == mtime && db_size == size
			{
				if root_id > 0 && db_root_id != root_id
				{
					music_db_update_song_root(root_id, &dir, &name);
				}
				skip += 1;
				continue;
			}
		}

		if let Ok(info) = probe_media_info_prefer_symphonia(&full_path)
		{
			//let has_cover = cover_probe_has_any(&info);
			music_db_upsert_song(root_id, &dir, &name, mtime, size, info.has_cover, &info);
			count += 1;
		}
	}
	sqlite3_exec(HDB_MUSIC, b"COMMIT;\0".as_ptr(), None, null_mut(), null_mut());
	eprintln!("[db] 目录扫描完成: {}, 新增/更新 {}, 跳过 {}", path, count, skip);
}

struct WatcherRoot {
	h_dir: i64,
	scan_root: String,
	root_id: i64,
	ov: Box<OVERLAPPED>, // Box 以确保地址固定
	buf: Vec<u8>,
}

// src\db_init.rs
// 歌曲库数据库模块 - 负责扫描目录并索引歌曲元数据

// 歌曲库数据库路径
const MUSIC_DB_PATH: &str = r"D:\float\disk\history\music.db";

static mut g_root_dir: String = String::new();

// 全局歌曲库数据库句柄
static mut HDB_MUSIC: i64 = 0;

struct MusicRootInfo {
	id: i64,
	path: String,
}

unsafe fn music_db_init() {
	let db_path = to_wstring(MUSIC_DB_PATH);

	if sqlite3_open16(db_path.as_ptr(), &mut HDB_MUSIC) != SQLITE_OK || HDB_MUSIC == 0
	{
		eprintln!("[db] 数据库打开失败: {}", MUSIC_DB_PATH);
		return;
	}

	// 创建歌曲索引表 (将 path 拆分为 dir 和 name)
	let sql_root = b"CREATE TABLE IF NOT EXISTS root (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT COLLATE NOCASE UNIQUE
        );\0";
	sqlite3_exec(HDB_MUSIC, sql_root.as_ptr(), None, null_mut(), null_mut());

	let sql_songs = b"CREATE TABLE IF NOT EXISTS songs (
                ID INTEGER PRIMARY KEY AUTOINCREMENT,
                root_id INTEGER DEFAULT 0,
                dir TEXT COLLATE NOCASE,
                name TEXT COLLATE NOCASE,
                modify_time INTEGER,
                size INTEGER,
                title TEXT COLLATE NOCASE,
                artist TEXT COLLATE NOCASE,
                album TEXT COLLATE NOCASE,
                duration_ms INTEGER,
                duration_text TEXT COLLATE NOCASE,
                codec TEXT,
                has_cover INTEGER DEFAULT 0,
                UNIQUE(dir, name)
        );\0";

	sqlite3_exec(HDB_MUSIC, sql_songs.as_ptr(), None, null_mut(), null_mut());
	// 创建索引加速目录删除和查询
	sqlite3_exec(HDB_MUSIC, b"CREATE INDEX IF NOT EXISTS idx_songs_dir ON songs (dir);\0".as_ptr(), None, null_mut(), null_mut());
	sqlite3_exec(HDB_MUSIC, b"CREATE INDEX IF NOT EXISTS idx_songs_root ON songs (root_id);\0".as_ptr(), None, null_mut(), null_mut());

	eprintln!("[db] 数据库已初始化 (dir/name 模式): {}", MUSIC_DB_PATH);
}

unsafe fn init_playlists_from_fog_db() {
	let playlists = db_load_playlists();
	let mut pool = m_pl_pool.write().unwrap();
	for (id, _name) in playlists
	{
		let songs = db_load_playlist_items(id);
		pool.insert(id, songs);
	}
}

/// 从数据库恢复播放状态
unsafe fn db_restore_playback() {
	// 开机启动模式：禁用自动播放 + 不显示窗口（通常用于开机自启后台运行）
	let is_startup_mode = std::env::args().any(|a| a == "-start");

	// 先恢复 UI 配置：窗口位置/可见性、音量、分隔比例（与播放列表无关）
	if let Some(cfg) = db_restore_ui_config()
	{
		g_to_volume.store(cfg.volume, Ordering::SeqCst);
		ui_volume_sync(cfg.volume);

		UI_SPLIT_LR.store(cfg.ui_split_lr.min(1000), Ordering::SeqCst);
		UI_SPLIT_LIST_LOG.store(cfg.ui_split_list_log.min(1000), Ordering::SeqCst);
		UI_SPLIT_COVER_TREE.store(cfg.ui_split_cover_tree.min(1000), Ordering::SeqCst);
		ui_set_playlist_col_ratios(cfg.ui_list_col_ratios);
		ui_mark_playlist_columns_pending_apply();

		if let Some(mut rect) = cfg.win_rect
		{
			// 限制位置不得小于 0*0，宽高限制最小 150*100
			rect[0] = rect[0].max(0);
			rect[1] = rect[1].max(0);
			rect[2] = rect[2].max(150);
			rect[3] = rect[3].max(100);
			ui_set_window_rect(rect[0], rect[1], rect[2], rect[3]);
		}
		let win_visible = cfg.win_visible && !is_startup_mode;
		ui_set_visible(win_visible);
		if win_visible
		{
			UpdateWindow(UI_HWND);
		}

		// 触发一次布局刷新（WM_SIZE 中读取分隔比例）
		PostMessageW(UI_HWND, WM_SIZE, 0, 0);
	}
	else
	{
		// First run: no saved UI config -> show by default (unless startup mode).
		let win_visible = !is_startup_mode;
		ui_set_visible(win_visible);
		if win_visible
		{
			UpdateWindow(UI_HWND);
		}
	}

	if let Some(state) = db_restore_state()
	{
		let volume = state.volume;
		let saved_track_idx = state.track_idx;
		let saved_track_path = state.track_path;
		let progress_ms = state.progress_ms;

		// 开机启动模式：禁用自动播放
		let is_playing = state.is_playing && !is_startup_mode;

		let li_id = state.playlist_id as usize;
		let songs = state.songs;

		// 恢复播放模式和音量
		let play_mode = db_load_playlist_play_mode(li_id);
		g_pl_mode.store(play_mode, Ordering::SeqCst);
		g_to_volume.store(volume, Ordering::SeqCst);
		ui_volume_sync(volume);

		// 恢复播放列表（li_id == fog.db playlist_id）
		let songs_for_ui = songs.clone();
		{
			let mut pool = m_pl_pool.write().unwrap();
			pool.insert(li_id, songs);
		}

		// 设置当前播放状态
		g_li_id.store(li_id, Ordering::SeqCst);
		ui_sync_playlist_tabs(li_id);
		let mut track_idx = saved_track_idx.min(songs_for_ui.len().saturating_sub(1));
		let mut resume_ms = progress_ms;
		if !saved_track_path.trim().is_empty()
		{
			let key = normalize_path_key(&saved_track_path);
			if let Some(i) = songs_for_ui
				.iter()
				.position(|s| normalize_path_key(&s.path) == key)
			{
				track_idx = i;
			}
			else
			{
				track_idx = 0;
				resume_ms = 0;
			}
		}
		g_track.store(track_idx, Ordering::SeqCst);
		g_to_pos_ms.store(resume_ms.min(u32::MAX as u64) as u32, Ordering::SeqCst);
		g_pl_is_changed.store(true, Ordering::SeqCst);

		// 如果上次是播放状态，则触发播放；如果是暂停，则设置为暂停并更新 UI
		if is_playing
		{
			// 触发播放
			if g_ev_li_chang != 0
			{
				SetEvent(g_ev_li_chang);
			}
		}
		else
		{
			// 恢复暂停状态：重置恢复事件，这样播放线程启动后会立即进入暂停等待状态
			ResetEvent(g_ev_resume);

			// 触发播放逻辑，让播放线程启动并加载文件，然后遇到 ResetEvent 就会暂停
			if g_ev_li_chang != 0
			{
				SetEvent(g_ev_li_chang);
			}

			// 更新 Pause 状态对应的 UI（但这通常由 player_thread 反馈，
			// 不过刚启动时 player_thread 可能还没跑起来，所以我们手动设一下?
			// player_thread 里的 smtc_update_playback_status 会发 WM_SMTC_STATUS 更新 UI
			// 但如果它一开始就是暂停的...
			// 我们可以手动发一个状态更新，确保 UI 显示为暂停
			set_player_state(PlayerState::Paused);
		}

		// 更新 UI
		ui_playlist_update(li_id, songs_for_ui.clone());
		ui_playlist_select(li_id, track_idx);
		// 更新窗口标题和播放项标记
		if let Some(song) = songs_for_ui.get(track_idx)
		{
			ui_set_now_playing2(li_id, track_idx, song);
		}

		eprintln!("[init] 从数据库恢复播放状态: playing={}", is_playing);
	}
}

/// 转换 FILETIME 为 u64
fn filetime_to_u64(ft: &FILETIME) -> u64 {
	((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

/// 转换文件大小为 u64
fn filesize_to_u64(high: u32, low: u32) -> u64 {
	((high as u64) << 32) | (low as u64)
}

/// 获取文件元数据标志
unsafe fn get_file_flags(path: &str) -> Option<(u64, u64)> {
	let mut p: WIN32_FIND_DATAW = zeroed();
	let h = FindFirstFileW(to_wstring(path).as_ptr(), &mut p);
	if h != -1
	{
		let mtime = filetime_to_u64(&p.ftLastWriteTime);
		let size = filesize_to_u64(p.nFileSizeHigh, p.nFileSizeLow);
		FindClose(h);
		Some((mtime, size))
	}
	else
	{
		None
	}
}

// src\db_listen.rs
/// 监听多个根目录变动 (单线程异步)
unsafe fn watch_db_dir(roots: &[MusicRootInfo]) {
	let mut items = Vec::with_capacity(roots.len());
	let mut handles = Vec::with_capacity(roots.len() + 1);

	handles.push(g_ev_app_quit);

	for root in roots
	{
		let h_dir = CreateFileW(
			to_wstring(&root.path).as_ptr(),
			FILE_LIST_DIRECTORY,
			FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
			0,
			OPEN_EXISTING,
			FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
			0,
		);

		if h_dir == -1
		{
			eprintln!("[db] 无法打开监控目录: {}", root.path);
			continue;
		}

		let h_event = CreateEventW(0, 1, 0, null());
		if h_event == 0
		{
			CloseHandle(h_dir);
			eprintln!("[db] 无法创建事件: {}", root.path);
			continue;
		}

		let mut ov = Box::new(zeroed::<OVERLAPPED>());
		ov.h_event = h_event;

		items.push(WatcherRoot { h_dir, scan_root: root.path.clone(), root_id: root.id, ov, buf: vec![0u8; 16384] });

		handles.push(h_event);
	}

	if items.is_empty()
	{
		return;
	}

	eprintln!("[db] 音乐库监控启动");

	unsafe fn watch_issue_read(item: &mut WatcherRoot) {
		ResetEvent(item.ov.h_event);
		let mut bytes_returned = 0u32;
		let ok = ReadDirectoryChangesW(
			item.h_dir,
			item.buf.as_mut_ptr(),
			item.buf.len() as u32,
			1,
			FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE,
			&mut bytes_returned,
			item.ov.as_mut() as *mut _ as i64,
			0,
		);

		if ok == 0
		{
			let err = GetLastError();
			if err != 997
			{
				eprintln!("[db] ReadDirectoryChangesW 失败 ({}): {}", item.scan_root, err);
			}
		}
	}

	#[derive(Clone)]
	struct WatchPending {
		full_path: String,
		root_id: i64,
		last_action: u32,
		due_tick: u64,
		retries: u8,
	}

	#[inline(always)]
	fn merge_watch_action(prev: u32, new_action: u32) -> u32 {
		if prev == FILE_ACTION_ADDED || prev == FILE_ACTION_RENAMED_NEW_NAME
		{
			return prev;
		}
		if new_action == FILE_ACTION_ADDED || new_action == FILE_ACTION_RENAMED_NEW_NAME
		{
			return new_action;
		}
		new_action
	}

	let debounce_ms: u64 = 250;
	let retry_delay_ms: u64 = 250;
	let retry_limit: u8 = 12;

	let mut pending: HashMap<String, WatchPending> = HashMap::default();

	// 初始发起所有目录监控请求
	for item in items.iter_mut()
	{
		watch_issue_read(item);
	}

	loop
	{
		let now = GetTickCount64();
		let next_due = pending
			.values()
			.map(|v| v.due_tick)
			.min();
		let timeout_ms =
			if let Some(due) = next_due { if due <= now { 0u32 } else { (due - now).min(0xFFFF_FFFF) as u32 } } else { 0xFFFF_FFFF };

		let wait_res = WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, timeout_ms);

		if wait_res == 0
		{
			eprintln!("[db] 监控线程退出");
			// 退出事件
			break;
		}

		// 超时：处理聚合后的待扫描项
		if wait_res == 258
		{
			let start_tick = GetTickCount64();

			let mut due_keys: Vec<String> = Vec::new();
			for (k, v) in pending.iter()
			{
				if v.due_tick <= start_tick
				{
					due_keys.push(k.clone());
				}
			}

			if !due_keys.is_empty()
			{
				let mut dirs: Vec<(String, WatchPending)> = Vec::new();
				let mut files: Vec<(String, WatchPending)> = Vec::new();
				let mut retry_count = 0usize;

				for key in due_keys
				{
					if let Some(mut p) = pending.remove(&key)
					{
						let attr = GetFileAttributesW(to_wstring(&p.full_path).as_ptr());
						if attr == INVALID_FILE_ATTRIBUTES
						{
							if p.retries < retry_limit
							{
								p.retries += 1;
								p.due_tick = start_tick + retry_delay_ms;
								pending.insert(key, p);
								retry_count += 1;
								continue;
							}
							eprintln!("[db] 监听重试放弃: {}", p.full_path);
							continue;
						}

						if (attr & FILE_ATTRIBUTE_DIRECTORY) != 0
						{
							if p.last_action != FILE_ACTION_MODIFIED
							{
								dirs.push((key, p));
							}
							continue;
						}

						let ext = p
							.full_path
							.rsplit_once('.')
							.map(|(_, e)| e.to_ascii_lowercase())
							.unwrap_or_default();

						if is_supported_media_ext(&ext)
						{
							files.push((key, p));
						}
					}
				}

				for (_, p) in dirs
				{
					music_db_scan_directory(&p.full_path, p.root_id, None);
				}

				let mut retry_pending: Vec<(String, WatchPending)> = Vec::new();
				let mut scanned_files = 0usize;

				if !files.is_empty()
				{
					sqlite3_exec(HDB_MUSIC, b"BEGIN TRANSACTION;\0".as_ptr(), None, null_mut(), null_mut());
					for (key, p) in files
					{
						scanned_files += 1;
						if !music_db_scan_file(&p.full_path, p.root_id)
						{
							let mut p = p;
							if p.retries < retry_limit
							{
								p.retries += 1;
								p.due_tick = start_tick + retry_delay_ms;
								retry_pending.push((key, p));
							}
							else
							{
								eprintln!("[db] 监听重试放弃: {}", p.full_path);
							}
						}
					}
					sqlite3_exec(HDB_MUSIC, b"COMMIT;\0".as_ptr(), None, null_mut(), null_mut());
				}

				retry_count += retry_pending.len();
				for (key, p) in retry_pending
				{
					pending.insert(key, p);
				}

				let cost = GetTickCount64().saturating_sub(start_tick);
				eprintln!("[db] 监控批量处理: 文件 {}, 重试 {}, 剩余 {}, 耗时 {}ms", scanned_files, retry_count, pending.len(), cost);
			}

			continue;
		}

		let idx = (wait_res - 1) as usize;
		if idx < items.len()
		{
			let item = &mut items[idx];
			let mut bytes_transferred = 0u32;
			if GetOverlappedResult(item.h_dir, item.ov.as_mut(), &mut bytes_transferred, 0) != 0 && bytes_transferred > 0
			{
				let now = GetTickCount64();
				let mut pos = 0;
				loop
				{
					let info = &item.buf[pos..] as *const _ as *const FILE_NOTIFY_INFORMATION;
					let name_len = (*info).FileNameLength as usize / 2;
					let name_ptr = (*info).FileName.as_ptr();
					let name = String::from_utf16_lossy(from_raw_parts(name_ptr, name_len));
					let action = (*info).Action;

					let full_path = format!("{}\\{}", item.scan_root, name);
					if action != FILE_ACTION_MODIFIED
					{
						eprintln!("[db] 监控事件: {}", full_path);
					}

					match action
					{
						FILE_ACTION_ADDED | FILE_ACTION_MODIFIED | FILE_ACTION_RENAMED_NEW_NAME =>
						{
							// 仅对“可能是媒体文件”的 MODIFIED 进行去抖；目录的 MODIFIED 很频繁且不应触发递归扫描
							let mut allow_queue = true;
							if action == FILE_ACTION_MODIFIED
							{
								let ext = name
									.rsplit_once('.')
									.map(|(_, e)| e.to_ascii_lowercase())
									.unwrap_or_default();
								allow_queue = is_supported_media_ext(&ext);
							}

							if allow_queue
							{
								let key = normalize_path_key(&full_path);
								let due_tick = now + debounce_ms;
								if let Some(p) = pending.get_mut(&key)
								{
									p.full_path = full_path;
									p.root_id = item.root_id;
									p.last_action = merge_watch_action(p.last_action, action);
									p.due_tick = due_tick;
								}
								else
								{
									pending.insert(
										key,
										WatchPending { full_path, root_id: item.root_id, last_action: action, due_tick, retries: 0 },
									);
								}
							}
						}
						FILE_ACTION_REMOVED | FILE_ACTION_RENAMED_OLD_NAME =>
						{
							// 路径已消失，无法通过磁盘判断类型，且为了原子性操作，使用单条 SQL 尝试删除文件或目录的所有相关记录
							let key = normalize_path_key(&full_path);
							let prefix = format!("{}\\", key);
							pending.retain(|k, _| k != &key && !k.starts_with(&prefix));

							music_db_remove_any(&full_path);
						}
						_ =>
						{}
					}
					if (*info).NextEntryOffset == 0
					{
						break;
					}
					pos += (*info).NextEntryOffset as usize;
				}
			}

			// 重新挂起该目录的监听请求（务必在任何重活之前）
			watch_issue_read(item);
		}
		else
		{
			// 异常或超时
			break;
		}
	}

	for item in items
	{
		CloseHandle(item.ov.h_event);
		CloseHandle(item.h_dir);
	}
}

/// 从数据库删除路径记录 (自动判断是文件还是目录)
unsafe fn music_db_remove_any(path: &str) {
	// 文件匹配逻辑参数
	let (dir, name) = split_path(path);

	// 目录匹配逻辑参数
	let base = if path.ends_with('\\') && path.len() > 3 { &path[..path.len() - 1] } else { path };
	let prefix = if base.ends_with('\\') { base.to_string() } else { format!("{}\\", base) };

	// 组合 SQL: 匹配 (dir=path_parent AND name=path_name) OR (dir=path_base OR dir LIKE path_base + '\%')
	let sql = b"DELETE FROM songs WHERE (dir = ? AND name = ?) OR (dir = ? OR instr(dir, ?) = 1);\0";
	let mut stmt: i64 = 0;

	if sqlite3_prepare_v2(HDB_MUSIC, sql.as_ptr(), sql.len(), &mut stmt, 0) == SQLITE_OK
	{
		sqlite3_bind_text(stmt, 1, dir.as_ptr(), dir.len(), -1);
		sqlite3_bind_text(stmt, 2, name.as_ptr(), name.len(), -1);
		sqlite3_bind_text(stmt, 3, base.as_ptr(), base.len(), -1);
		sqlite3_bind_text(stmt, 4, prefix.as_ptr(), prefix.len(), -1);

		sqlite3_step(stmt);

		if sqlite3_changes(HDB_MUSIC) > 0
		{
			eprintln!("[db] 已移除记录: {}", path);
		}

		sqlite3_finalize(stmt);
	}
}

/// 扫描单个文件入口
/// 返回：true=已处理/已跳过(无需重试)，false=需稍后重试(文件可能仍在写入/锁定)
unsafe fn music_db_scan_file(path: &str, root_id: i64) -> bool {
	if let Some((mtime, size)) = get_file_flags(path)
	{
		let (dir, name) = split_path(path);
		if let Some((db_mtime, db_size, db_root_id)) = music_db_get_file_flags(dir, name)
		{
			if db_mtime == mtime && db_size == size
			{
				if root_id > 0 && db_root_id != root_id
				{
					music_db_update_song_root(root_id, dir, name);
				}
				return true;
			}
		}

		if let Ok(info) = probe_media_info_prefer_symphonia(path)
		{
			//let has_cover = cover_probe_has_any(&info);
			music_db_upsert_song(root_id, dir, name, mtime, size, info.has_cover, &info);
			eprintln!("[db] 更新单文件记录: {}", path);
			return true;
		}

		return false;
	}

	false
}

// src\dec_ape_mac.rs
static mut mac_create_w: extern "system" fn(*const u16, *mut i32) -> i64 = __create_w;
static mut mac_destroy: extern "system" fn(i64) = __destroy;
static mut mac_get_data: extern "system" fn(i64, *mut u8, i64, *mut i64) -> i32 = __get_data;
static mut mac_seek: extern "system" fn(i64, i64) -> i32 = __seek;
static mut mac_get_info: extern "system" fn(i64, i32, i64, i64) -> i64 = __get_info;

extern "system" fn __create_w(_: *const u16, _: *mut i32) -> i64 {
	0
}
extern "system" fn __destroy(_: i64) {}
extern "system" fn __get_data(_: i64, _: *mut u8, _: i64, _: *mut i64) -> i32 {
	0
}
extern "system" fn __seek(_: i64, _: i64) -> i32 {
	0
}
extern "system" fn __get_info(_: i64, _: i32, _: i64, _: i64) -> i64 {
	0
}

struct MacDecoder {
	handle: i64,
	sample_rate: u32,
	channels: u16,
	bits_per_sample: u16,
	duration_ms: u64,
}

impl MacDecoder {
	unsafe fn new(path: &str) -> Result<Self, String> {
		let w_path: Vec<u16> = path
			.encode_utf16()
			.chain(Some(0))
			.collect();
		let mut error_code = 0;

		let handle = mac_create_w(w_path.as_ptr(), &mut error_code);
		if handle == 0 || error_code != 0
		{
			return Err(format!("Failed to open APE file: error code {}", error_code));
		}

		let sample_rate = mac_get_info(handle, 1003, 0, 0) as u32; // APE_INFO_SAMPLE_RATE = 1003
		let channels = mac_get_info(handle, 1006, 0, 0) as u16; // APE_INFO_CHANNELS = 1006
		let bits_per_sample = mac_get_info(handle, 1004, 0, 0) as u16; // APE_INFO_BITS_PER_SAMPLE = 1004
		let duration_ms = mac_get_info(handle, 1017, 0, 0) as u64; // APE_INFO_LENGTH_MS = 1017

		Ok(MacDecoder { handle, sample_rate, channels, bits_per_sample, duration_ms })
	}

	unsafe fn decode_blocks(&mut self, blocks_to_read: usize) -> Result<Vec<f64>, String> {
		let bytes_per_sample = self.bits_per_sample / 8;
		let block_align = bytes_per_sample as usize * self.channels as usize;
		let buffer_size = blocks_to_read * block_align;

		let mut buffer = vec![0u8; buffer_size];
		let mut blocks_retrieved: i64 = 0;

		let ret = mac_get_data(self.handle, buffer.as_mut_ptr(), blocks_to_read as i64, &mut blocks_retrieved);
		if ret != 0
		{
			return Err(format!("Read error: {}", ret));
		}

		if blocks_retrieved == 0
		{
			return Ok(Vec::new()); // EOF
		}

		let count = (blocks_retrieved as usize) * (self.channels as usize);
		let mut output = Vec::with_capacity(count);

		match self.bits_per_sample
		{
			16 =>
			{
				let ptr = buffer.as_ptr() as *const i16;
				for i in 0..count
				{
					let s = *ptr.add(i);
					output.push(s as f64 / 32768.0);
				}
			}
			24 =>
			{
				let ptr = buffer.as_ptr();
				for i in 0..count
				{
					let offset = i * 3;
					let b1 = *ptr.add(offset) as i32;
					let b2 = *ptr.add(offset + 1) as i32;
					let b3 = *ptr.add(offset + 2) as i32;
					let val = b1 | (b2 << 8) | (b3 << 16);
					let signed_val = (val << 8) >> 8;
					output.push(signed_val as f64 / 8388608.0);
				}
			}
			8 =>
			{
				let ptr = buffer.as_ptr();
				for i in 0..count
				{
					let s = *ptr.add(i);
					output.push((s as f64 - 128.0) / 128.0);
				}
			}
			_ => return Err(format!("Unsupported bit depth: {}", self.bits_per_sample)),
		}

		Ok(output)
	}

	unsafe fn seek_blocks(&mut self, block_offset: i64) -> Result<(), String> {
		let ret = mac_seek(self.handle, block_offset);
		if ret != 0 { Err(format!("Seek error: {}", ret)) } else { Ok(()) }
	}
}

impl Drop for MacDecoder {
	fn drop(&mut self) {
		unsafe {
			mac_destroy(self.handle);
		}
	}
}

// src\dec_ffmpeg.rs
// === FFmpeg 解码器 ===
#[derive(Clone, Default)]
struct FFmpegProbeMediaInfo {
	duration_ms: u64,
	codec_name: String,
	tags: Vec<(String, String)>,
	has_cover: bool,
}

fn ffmpeg_probe_media_info(path: &str) -> Result<FFmpegProbeMediaInfo, String> {
	unsafe {
		let funcs = AV_FUNCS
			.as_ref()
			.ok_or("FFmpeg DLL not loaded")?;

		let mut ctx: *mut AVFormatContext = null_mut();
		let c_path = CString::new(path).map_err(|_| "invalid path")?;

		if (funcs.avformat_open_input)(&mut ctx, c_path.as_ptr(), null(), null_mut()) != 0
		{
			return Err("avformat_open_input failed".to_string());
		}

		let _ = (funcs.avformat_find_stream_info)(ctx, null_mut());

		// Detect embedded cover (attached picture stream)
		let mut has_cover = false;
		let nb = (*ctx).nb_streams as isize;
		let streams = (*ctx).streams;
		if !streams.is_null() && nb > 0
		{
			for i in 0..nb
			{
				let st = *streams.offset(i);
				if st.is_null()
				{
					continue;
				}
				if ((*st).disposition & AV_DISPOSITION_ATTACHED_PIC) != 0 || (*st).attached_pic.size > 0
				{
					has_cover = true;
					break;
				}
			}
		}

		// Duration (AV_TIME_BASE = 1_000_000)
		let duration_us = (*ctx).duration;
		let duration_ms = if duration_us > 0 { (duration_us / 1000) as u64 } else { 0 };

		let mut tags: Vec<(String, String)> = Vec::new();
		tags.extend(ffmpeg_dict_collect(funcs, (*ctx).metadata as *const AVDictionary));

		let mut codec_name: String = String::new();
		let mut audio_stream: *mut AVStream = null_mut();

		// Prefer av_find_best_stream (also returns codec)
		let mut best_codec: *const AVCodec = null();
		let best_idx = (funcs.av_find_best_stream)(ctx, AVMEDIA_TYPE_AUDIO, -1, -1, &mut best_codec, 0);
		if best_idx >= 0
		{
			let streams = (*ctx).streams;
			if !streams.is_null()
			{
				audio_stream = *streams.offset(best_idx as isize);
			}
			if !best_codec.is_null() && !(*best_codec).name.is_null()
			{
				codec_name = CStr::from_ptr((*best_codec).name)
					.to_string_lossy()
					.into_owned();
			}
		}

		if audio_stream.is_null()
		{
			let nb = (*ctx).nb_streams as isize;
			let streams = (*ctx).streams;
			if !streams.is_null()
			{
				for i in 0..nb
				{
					let st = *streams.offset(i);
					if st.is_null()
					{
						continue;
					}
					let codecpar = (*st).codecpar;
					if !codecpar.is_null() && (*codecpar).codec_type == AVMEDIA_TYPE_AUDIO
					{
						audio_stream = st;
						break;
					}
				}
			}
		}

		if !audio_stream.is_null()
		{
			tags.extend(ffmpeg_dict_collect(funcs, (*audio_stream).metadata as *const AVDictionary));

			if codec_name.is_empty()
			{
				let codecpar = (*audio_stream).codecpar;
				if !codecpar.is_null()
				{
					let codec_id = (*codecpar).codec_id;
					let codec = (funcs.avcodec_find_decoder)(codec_id);
					if !codec.is_null() && !(*codec).name.is_null()
					{
						codec_name = CStr::from_ptr((*codec).name)
							.to_string_lossy()
							.into_owned();
					}
				}
			}
		}

		(funcs.avformat_close_input)(&mut ctx);
		Ok(FFmpegProbeMediaInfo { duration_ms, codec_name, tags, has_cover })
	}
}

// === PCM 二次解码: DTS-in-PCM (DTS-CD 等) ===

struct FfmpegDtsInPcmDecoder {
	codec_ctx: *mut AVCodecContext,
	parser: *mut AVCodecParserContext,
	pkt: *mut AVPacket,
	frame: *mut AVFrame,
	codec_id: i32,
	input_buf: Vec<u8>,
	input_pos: usize,
	input_eof: bool,
	sent_eof: bool,
}

impl FfmpegDtsInPcmDecoder {
	unsafe fn new() -> Result<Self, String> {
		let funcs = AV_FUNCS
			.as_ref()
			.ok_or("FFmpeg DLL 未加载")?;

		let name = CString::new("dca").map_err(|_| "invalid codec name")?;
		let codec = (funcs.avcodec_find_decoder_by_name)(name.as_ptr());
		if codec.is_null()
		{
			return Err("找不到 DTS(dca) 解码器".to_string());
		}

		let codec_ctx = (funcs.avcodec_alloc_context3)(codec);
		if codec_ctx.is_null()
		{
			return Err("avcodec_alloc_context3 失败".to_string());
		}

		if (funcs.avcodec_open2)(codec_ctx, codec, null_mut()) < 0
		{
			(funcs.avcodec_free_context)(&mut (codec_ctx as *mut _));
			return Err("avcodec_open2(DTS) 失败".to_string());
		}

		let codec_id = (*codec_ctx).codec_id;
		let parser = (funcs.av_parser_init)(codec_id);
		if parser.is_null()
		{
			(funcs.avcodec_free_context)(&mut (codec_ctx as *mut _));
			return Err("av_parser_init(DTS) 失败".to_string());
		}

		let pkt = (funcs.av_packet_alloc)();
		let frame = (funcs.av_frame_alloc)();
		if pkt.is_null() || frame.is_null()
		{
			if !pkt.is_null()
			{
				(funcs.av_packet_free)(&mut (pkt as *mut _));
			}
			if !frame.is_null()
			{
				(funcs.av_frame_free)(&mut (frame as *mut _));
			}
			(funcs.av_parser_close)(parser);
			(funcs.avcodec_free_context)(&mut (codec_ctx as *mut _));
			return Err("av_packet_alloc/av_frame_alloc 失败".to_string());
		}

		Ok(FfmpegDtsInPcmDecoder {
			codec_ctx,
			parser,
			pkt,
			frame,
			codec_id,
			input_buf: Vec::with_capacity(256 * 1024),
			input_pos: 0,
			input_eof: false,
			sent_eof: false,
		})
	}

	fn push_s16le_words<I>(&mut self, words: I)
	where
		I: IntoIterator<Item = u16>,
	{
		for w in words
		{
			let b = w.to_le_bytes();
			self.input_buf.push(b[0]);
			self.input_buf.push(b[1]);
		}
	}

	fn set_input_eof(&mut self) {
		self.input_eof = true;
	}

	unsafe fn reset(&mut self) -> Result<(), String> {
		let funcs = AV_FUNCS
			.as_ref()
			.ok_or("FFmpeg DLL 未加载")?;

		(funcs.avcodec_flush_buffers)(self.codec_ctx);
		(funcs.av_packet_unref)(self.pkt);
		(funcs.av_frame_unref)(self.frame);

		if !self.parser.is_null()
		{
			(funcs.av_parser_close)(self.parser);
		}
		self.parser = (funcs.av_parser_init)(self.codec_id);
		if self.parser.is_null()
		{
			return Err("av_parser_init(DTS) 失败".to_string());
		}

		self.input_buf.clear();
		self.input_pos = 0;
		self.input_eof = false;
		self.sent_eof = false;
		Ok(())
	}

	unsafe fn try_decode(&mut self) -> Result<Option<Vec<f64>>, String> {
		const AV_NOPTS_VALUE: i64 = i64::MIN;
		const AVERROR_EOF: i32 = -541478725;
		const AVERROR_EAGAIN: i32 = -11;

		let funcs = AV_FUNCS
			.as_ref()
			.ok_or("FFmpeg DLL 未加载")?;

		loop
		{
			let ret = (funcs.avcodec_receive_frame)(self.codec_ctx, self.frame);
			if ret == 0
			{
				let nb_samples = (*self.frame).nb_samples;
				if nb_samples <= 0
				{
					(funcs.av_frame_unref)(self.frame);
					continue;
				}

				let out = self.frame_to_stereo_f64();
				(funcs.av_frame_unref)(self.frame);
				return out.map(Some);
			}
			if ret == AVERROR_EOF
			{
				return Ok(Some(Vec::new()));
			}
			if ret != AVERROR_EAGAIN
			{
				return Err(format!("DTS avcodec_receive_frame 失败: {}", ret));
			}

			// EAGAIN: 需要更多输入
			if self.input_pos >= self.input_buf.len()
			{
				if self.input_eof
				{
					if !self.sent_eof
					{
						let r = (funcs.avcodec_send_packet)(self.codec_ctx, null());
						self.sent_eof = true;
						if r < 0
						{
							return Err(format!("DTS flush avcodec_send_packet 失败: {}", r));
						}
						continue;
					}
					return Ok(Some(Vec::new()));
				}
				else
				{
					return Ok(None);
				}
			}

			let in_ptr = self
				.input_buf
				.as_ptr()
				.add(self.input_pos);
			let in_size = (self.input_buf.len() - self.input_pos) as i32;

			let mut out_data: *mut u8 = null_mut();
			let mut out_size: i32 = 0;
			let used = (funcs.av_parser_parse2)(
				self.parser,
				self.codec_ctx,
				&mut out_data,
				&mut out_size,
				in_ptr,
				in_size,
				AV_NOPTS_VALUE,
				AV_NOPTS_VALUE,
				0,
			);
			if used < 0
			{
				return Err(format!("DTS av_parser_parse2 失败: {}", used));
			}
			self.input_pos = self
				.input_pos
				.saturating_add(used as usize);

			if out_size > 0
			{
				(funcs.av_packet_unref)(self.pkt);
				(*self.pkt).data = out_data;
				(*self.pkt).size = out_size;
				(*self.pkt).pts = AV_NOPTS_VALUE;
				(*self.pkt).dts = AV_NOPTS_VALUE;
				(*self.pkt).stream_index = 0;

				let r = (funcs.avcodec_send_packet)(self.codec_ctx, self.pkt);
				if r < 0
				{
					return Err(format!("DTS avcodec_send_packet 失败: {}", r));
				}
			}
			else if used == 0
			{
				// parser 需要更多数据才能输出完整 packet
				if self.input_eof
				{
					if !self.sent_eof
					{
						let r = (funcs.avcodec_send_packet)(self.codec_ctx, null());
						self.sent_eof = true;
						if r < 0
						{
							return Err(format!("DTS flush avcodec_send_packet 失败: {}", r));
						}
						continue;
					}
					return Ok(Some(Vec::new()));
				}
				else
				{
					return Ok(None);
				}
			}

			if self.input_pos > 0 && self.input_pos >= 256 * 1024
			{
				self.input_buf.drain(..self.input_pos);
				self.input_pos = 0;
			}
		}
	}

	unsafe fn frame_to_stereo_f64(&self) -> Result<Vec<f64>, String> {
		let nb_samples = (*self.frame).nb_samples;

		let src_channels = (*self.codec_ctx).ch_layout.nb_channels;
		let src_channels = if src_channels > 0 { src_channels } else { 2 };

		let format = (*self.frame).format;
		let data_ptr = if !(*self.frame).extended_data.is_null()
		{
			(*self.frame).extended_data
		}
		else
		{
			(*self.frame).data.as_ptr() as *mut *mut u8
		};

		if data_ptr.is_null() || (*data_ptr.offset(0)).is_null()
		{
			return Err("DTS 解码帧数据为空".to_string());
		}

		let mut out = Vec::with_capacity(nb_samples as usize * 2);

		match format
		{
			1 =>
			{
				// AV_SAMPLE_FMT_S16 (packed)
				let ptr = *data_ptr.offset(0) as *const i16;
				for i in 0..nb_samples
				{
					let base = (i * src_channels) as isize;
					let l = *ptr.offset(base);
					let r = if src_channels > 1 { *ptr.offset(base + 1) } else { l };
					out.push(l as f64 / 32768.0);
					out.push(r as f64 / 32768.0);
				}
			}
			6 =>
			{
				// AV_SAMPLE_FMT_S16P (planar)
				let ch0 = *data_ptr.offset(0) as *const i16;
				let ch1 = if src_channels > 1 && !(*data_ptr.offset(1)).is_null()
				{
					*data_ptr.offset(1) as *const i16
				}
				else
				{
					ch0
				};
				for i in 0..nb_samples
				{
					let l = *ch0.offset(i as isize);
					let r = *ch1.offset(i as isize);
					out.push(l as f64 / 32768.0);
					out.push(r as f64 / 32768.0);
				}
			}
			2 =>
			{
				// AV_SAMPLE_FMT_S32 (packed)
				let ptr = *data_ptr.offset(0) as *const i32;
				for i in 0..nb_samples
				{
					let base = (i * src_channels) as isize;
					let l = *ptr.offset(base);
					let r = if src_channels > 1 { *ptr.offset(base + 1) } else { l };
					out.push(l as f64 / 2147483648.0);
					out.push(r as f64 / 2147483648.0);
				}
			}
			7 =>
			{
				// AV_SAMPLE_FMT_S32P (planar)
				let ch0 = *data_ptr.offset(0) as *const i32;
				let ch1 = if src_channels > 1 && !(*data_ptr.offset(1)).is_null()
				{
					*data_ptr.offset(1) as *const i32
				}
				else
				{
					ch0
				};
				for i in 0..nb_samples
				{
					let l = *ch0.offset(i as isize);
					let r = *ch1.offset(i as isize);
					out.push(l as f64 / 2147483648.0);
					out.push(r as f64 / 2147483648.0);
				}
			}
			3 =>
			{
				// AV_SAMPLE_FMT_FLT (packed)
				let ptr = *data_ptr.offset(0) as *const f32;
				for i in 0..nb_samples
				{
					let base = (i * src_channels) as isize;
					let l = *ptr.offset(base) as f64;
					let r = if src_channels > 1 { *ptr.offset(base + 1) as f64 } else { l };
					out.push(l);
					out.push(r);
				}
			}
			8 =>
			{
				// AV_SAMPLE_FMT_FLTP (planar)
				let ch0 = *data_ptr.offset(0) as *const f32;
				let ch1 = if src_channels > 1 && !(*data_ptr.offset(1)).is_null()
				{
					*data_ptr.offset(1) as *const f32
				}
				else
				{
					ch0
				};
				for i in 0..nb_samples
				{
					let l = *ch0.offset(i as isize) as f64;
					let r = *ch1.offset(i as isize) as f64;
					out.push(l);
					out.push(r);
				}
			}
			4 =>
			{
				// AV_SAMPLE_FMT_DBL (packed)
				let ptr = *data_ptr.offset(0) as *const f64;
				for i in 0..nb_samples
				{
					let base = (i * src_channels) as isize;
					let l = *ptr.offset(base);
					let r = if src_channels > 1 { *ptr.offset(base + 1) } else { l };
					out.push(l);
					out.push(r);
				}
			}
			9 =>
			{
				// AV_SAMPLE_FMT_DBLP (planar)
				let ch0 = *data_ptr.offset(0) as *const f64;
				let ch1 = if src_channels > 1 && !(*data_ptr.offset(1)).is_null()
				{
					*data_ptr.offset(1) as *const f64
				}
				else
				{
					ch0
				};
				for i in 0..nb_samples
				{
					let l = *ch0.offset(i as isize);
					let r = *ch1.offset(i as isize);
					out.push(l);
					out.push(r);
				}
			}
			_ => return Err(format!("DTS 不支持的采样格式: {}", format)),
		}

		Ok(out)
	}
}

impl Drop for FfmpegDtsInPcmDecoder {
	fn drop(&mut self) {
		unsafe {
			if let Some(funcs) = AV_FUNCS.as_ref()
			{
				if !self.parser.is_null()
				{
					(funcs.av_parser_close)(self.parser);
					self.parser = null_mut();
				}
				let mut codec_ctx = self.codec_ctx;
				let mut pkt = self.pkt;
				let mut frame = self.frame;
				(funcs.avcodec_free_context)(&mut codec_ctx);
				(funcs.av_packet_free)(&mut pkt);
				(funcs.av_frame_free)(&mut frame);
			}
		}
	}
}

struct FFmpegDecoder {
	ctx: *mut AVFormatContext,
	codec_ctx: *mut AVCodecContext,
	pkt: *mut AVPacket,
	frame: *mut AVFrame,
	stream_idx: i32,
	stream_time_base: AVRational,
	stream_first_ts: i64,
	sample_rate: u32,
	channels: u16,
	bits_per_sample: u16,
	duration_ms: u64,
	pcm_probe_checked: bool,
	dts_in_pcm: Option<FfmpegDtsInPcmDecoder>,
}

impl FFmpegDecoder {
	unsafe fn ms_to_stream_ts(ms: i64, tb: AVRational) -> Option<i64> {
		if tb.num <= 0 || tb.den <= 0
		{
			return None;
		}
		let num = tb.num as i128;
		let den = tb.den as i128;
		let ms = ms as i128;
		let ts = (ms * den) / (num * 1000);
		Some(ts as i64)
	}

	unsafe fn stream_ts_to_ms(ts: i64, tb: AVRational) -> Option<i64> {
		if tb.num <= 0 || tb.den <= 0
		{
			return None;
		}
		let num = tb.num as i128;
		let den = tb.den as i128;
		let ts = ts as i128;
		let ms = (ts * num * 1000) / den;
		Some(ms as i64)
	}

	unsafe fn detect_stream_first_ts(ctx: *mut AVFormatContext, stream_idx: i32, tb: AVRational, pkt: *mut AVPacket) -> i64 {
		const AV_NOPTS_VALUE: i64 = i64::MIN;
		const AVSEEK_FLAG_BACKWARD: i32 = 1;
		const AVSEEK_FLAG_ANY: i32 = 4;

		let Some(funcs) = AV_FUNCS.as_ref()
		else
		{
			return 0;
		};

		let mut first_ts: Option<i64> = None;
		for _ in 0..256
		{
			let r = (funcs.av_read_frame)(ctx, pkt);
			if r < 0
			{
				break;
			}

			if (*pkt).stream_index == stream_idx
			{
				let pts = (*pkt).pts;
				let dts = (*pkt).dts;
				if pts != AV_NOPTS_VALUE
				{
					first_ts = Some(pts);
				}
				else if dts != AV_NOPTS_VALUE
				{
					first_ts = Some(dts);
				}

				(funcs.av_packet_unref)(pkt);
				break;
			}

			(funcs.av_packet_unref)(pkt);
		}

		let first_ts = first_ts.unwrap_or(0);

		// 回到曲首（如果该文件的 packet 时间戳不是从 0 开始，seek 也要使用同一时间基准）
		let r = (funcs.av_seek_frame)(ctx, stream_idx, first_ts, AVSEEK_FLAG_BACKWARD | AVSEEK_FLAG_ANY);
		if r < 0
		{
			// 不可 seek 的输入：尽量回退到开头，避免消耗掉首包导致曲首被截断
			let _ = (funcs.av_seek_frame)(ctx, -1, 0, AVSEEK_FLAG_BACKWARD | AVSEEK_FLAG_ANY);
		}

		if first_ts != 0
			&& let Some(ms) = Self::stream_ts_to_ms(first_ts, tb)
		{
			eprintln!("[FFmpeg] stream_first_ts={} ({}ms)", first_ts, ms);
		}

		first_ts
	}

	unsafe fn new(path: &str) -> Result<Self, String> {
		let funcs = AV_FUNCS
			.as_ref()
			.ok_or("FFmpeg DLL 未加载")?;

		let mut ctx: *mut AVFormatContext = null_mut();
		let c_path = CString::new(path).map_err(|_| "无效路径")?;

		if (funcs.avformat_open_input)(&mut ctx, c_path.as_ptr(), null(), null_mut()) != 0
		{
			return Err("avformat_open_input 失败".to_string());
		}

		if (funcs.avformat_find_stream_info)(ctx, null_mut()) < 0
		{
			(funcs.avformat_close_input)(&mut ctx);
			return Err("avformat_find_stream_info 失败".to_string());
		}

		// 获取时长 (单位: 微秒 AV_TIME_BASE)
		let duration_us = (*ctx).duration;
		let duration_ms = if duration_us > 0 { (duration_us / 1000) as u64 } else { 0 };

		// 获取流数量
		let nb_streams = (*ctx).nb_streams;
		eprintln!("[FFmpeg] nb_streams={}, duration={}ms", nb_streams, duration_ms);

		if nb_streams == 0
		{
			(funcs.avformat_close_input)(&mut ctx);
			return Err("文件中没有流".to_string());
		}

		//先用 av_find_best_stream 选择音频流，失败则遍历 streams 找到第一个 AVMEDIA_TYPE_AUDIO，找不到就返回错误
		let mut best_codec: *const AVCodec = null();
		let mut stream_idx: i32 = (funcs.av_find_best_stream)(ctx, AVMEDIA_TYPE_AUDIO, -1, -1, &mut best_codec, 0);
		if stream_idx < 0
		{
			let streams = (*ctx).streams;
			if streams.is_null()
			{
				(funcs.avformat_close_input)(&mut ctx);
				return Err("FFmpeg streams 为空".to_string());
			}

			stream_idx = -1;
			for i in 0..nb_streams
			{
				let st = *streams.offset(i as isize);
				if st.is_null()
				{
					continue;
				}
				let codecpar = (*st).codecpar;
				if !codecpar.is_null() && (*codecpar).codec_type == AVMEDIA_TYPE_AUDIO
				{
					stream_idx = i as i32;
					break;
				}
			}
		}
		if stream_idx < 0 || stream_idx as u32 >= nb_streams
		{
			(funcs.avformat_close_input)(&mut ctx);
			return Err("未找到音频流".to_string());
		}
		eprintln!("[FFmpeg] 使用流索引: {}", stream_idx);

		// 获取 stream->codecpar
		let streams = (*ctx).streams;
		if streams.is_null()
		{
			(funcs.avformat_close_input)(&mut ctx);
			return Err("FFmpeg streams 为空".to_string());
		}
		let stream = *streams.offset(stream_idx as isize);
		if stream.is_null()
		{
			(funcs.avformat_close_input)(&mut ctx);
			return Err("FFmpeg stream 为空".to_string());
		}
		let codecpar = (*stream).codecpar;
		if codecpar.is_null()
		{
			(funcs.avformat_close_input)(&mut ctx);
			return Err("FFmpeg codecpar 为空".to_string());
		}

		// 从 codecpar 获取 codec_id
		let codec_id = (*codecpar).codec_id;
		eprintln!("[FFmpeg] codec_id: {}", codec_id);

		// 使用 avcodec_find_decoder 获取解码器
		let codec = (funcs.avcodec_find_decoder)(codec_id);
		if codec.is_null()
		{
			(funcs.avformat_close_input)(&mut ctx);
			return Err(format!("找不到 codec_id={} 的解码器", codec_id));
		}

		let codec_ctx = (funcs.avcodec_alloc_context3)(codec);
		if codec_ctx.is_null()
		{
			(funcs.avformat_close_input)(&mut ctx);
			return Err("avcodec_alloc_context3 失败".to_string());
		}

		if (funcs.avcodec_parameters_to_context)(codec_ctx, codecpar) < 0
		{
			(funcs.avcodec_free_context)(&mut (codec_ctx as *mut _));
			(funcs.avformat_close_input)(&mut ctx);
			return Err("avcodec_parameters_to_context 失败".to_string());
		}

		if (funcs.avcodec_open2)(codec_ctx, codec, null_mut()) < 0
		{
			(funcs.avcodec_free_context)(&mut (codec_ctx as *mut _));
			(funcs.avformat_close_input)(&mut ctx);
			return Err("avcodec_open2 失败".to_string());
		}

		// 从 AVCodecContext 获取参数
		// 已更新为使用字段访问
		let sample_rate = (*codec_ctx).sample_rate as u32;
		let channels = (*codec_ctx).ch_layout.nb_channels;
		let channels = if channels > 0 && channels <= 32 { channels as u16 } else { 2 };

		// 优先使用 codecpar 提供的原始位深；缺失时标记为未知(0)
		let bits_per_sample = {
			let raw = (*codecpar).bits_per_raw_sample;
			let coded = (*codecpar).bits_per_coded_sample;
			let b = if raw > 0
			{
				raw
			}
			else if coded > 0
			{
				coded
			}
			else
			{
				0
			};
			let b = if (8..=32).contains(&b) { b } else { 0 };
			if b == 0 { 0 } else { b as u16 }
		};

		let pkt = (funcs.av_packet_alloc)();
		let frame = (funcs.av_frame_alloc)();

		let stream_time_base = (*stream).time_base;
		let stream_first_ts = Self::detect_stream_first_ts(ctx, stream_idx, stream_time_base, pkt);

		eprintln!("[FFmpeg] 打开: {} ({}Hz/{}ch/{}ms)", path, sample_rate, channels, duration_ms);

		Ok(FFmpegDecoder {
			ctx,
			codec_ctx,
			pkt,
			frame,
			stream_idx,
			stream_time_base,
			stream_first_ts,
			sample_rate,
			channels,
			bits_per_sample,
			duration_ms,
			pcm_probe_checked: false,
			dts_in_pcm: None,
		})
	}

	unsafe fn receive_next_frame(&mut self, funcs: &AvFunctions) -> Result<bool, String> {
		const AVERROR_EOF: i32 = -541478725;
		const AVERROR_EAGAIN: i32 = -11;

		loop
		{
			let ret = (funcs.avcodec_receive_frame)(self.codec_ctx, self.frame);
			if ret == 0
			{
				return Ok(true);
			}
			if ret == AVERROR_EAGAIN
			{
				// need more input
			}
			else if ret == AVERROR_EOF
			{
				return Ok(false);
			}
			else
			{
				return Err(format!("接收帧错误: {}", ret));
			}

			let read_ret = (funcs.av_read_frame)(self.ctx, self.pkt);
			if read_ret < 0
			{
				// EOF 或错误，发送 flush
				(funcs.avcodec_send_packet)(self.codec_ctx, null());
				continue;
			}

			if (*self.pkt).stream_index == self.stream_idx
			{
				(funcs.avcodec_send_packet)(self.codec_ctx, self.pkt);
			}
			(funcs.av_packet_unref)(self.pkt);
		}
	}

	unsafe fn feed_dts_in_pcm_from_frame(&mut self) -> Result<(), String> {
		let Some(ref mut dts) = self.dts_in_pcm
		else
		{
			return Err("DTS-in-PCM 解码器未初始化".to_string());
		};

		let nb_samples = (*self.frame).nb_samples;
		if nb_samples <= 0
		{
			return Ok(());
		}

		let format = (*self.frame).format;
		let data_ptr = if !(*self.frame).extended_data.is_null()
		{
			(*self.frame).extended_data
		}
		else
		{
			(*self.frame).data.as_ptr() as *mut *mut u8
		};

		let channels = self.channels as i32;
		if channels < 2
		{
			return Err("DTS-in-PCM 需要 2ch PCM".to_string());
		}

		match format
		{
			1 =>
			{
				// AV_SAMPLE_FMT_S16 (packed)
				if data_ptr.is_null() || (*data_ptr.offset(0)).is_null()
				{
					return Err("FFmpeg 解码帧数据为空".to_string());
				}
				let ptr = *data_ptr.offset(0) as *const u16;
				let words = from_raw_parts(ptr, (nb_samples * channels) as usize);
				dts.push_s16le_words(words.iter().copied());
				Ok(())
			}
			6 =>
			{
				// AV_SAMPLE_FMT_S16P (planar)
				if data_ptr.is_null() || (*data_ptr.offset(0)).is_null() || (*data_ptr.offset(1)).is_null()
				{
					return Err("FFmpeg 解码帧数据为空".to_string());
				}
				let ch0_ptr = *data_ptr.offset(0) as *const u16;
				let ch1_ptr = *data_ptr.offset(1) as *const u16;
				let ch0 = from_raw_parts(ch0_ptr, nb_samples as usize);
				let ch1 = from_raw_parts(ch1_ptr, nb_samples as usize);
				let mut i = 0usize;
				let mut right = false;
				let words = std::iter::from_fn(|| {
					if i >= nb_samples as usize
					{
						return None;
					}
					let w = if !right
					{
						right = true;
						ch0[i]
					}
					else
					{
						right = false;
						let w = ch1[i];
						i = i.saturating_add(1);
						w
					};
					Some(w)
				});
				dts.push_s16le_words(words);
				Ok(())
			}
			2 =>
			{
				// AV_SAMPLE_FMT_S32 (packed)
				if data_ptr.is_null() || (*data_ptr.offset(0)).is_null()
				{
					return Err("FFmpeg 解码帧数据为空".to_string());
				}
				let ptr = *data_ptr.offset(0) as *const i32;
				let words32 = from_raw_parts(ptr, (nb_samples * channels) as usize);
				dts.push_s16le_words(words32.iter().map(|&s| ((s >> 16) as i16) as u16));
				Ok(())
			}
			7 =>
			{
				// AV_SAMPLE_FMT_S32P (planar)
				if data_ptr.is_null() || (*data_ptr.offset(0)).is_null() || (*data_ptr.offset(1)).is_null()
				{
					return Err("FFmpeg 解码帧数据为空".to_string());
				}
				let ch0_ptr = *data_ptr.offset(0) as *const i32;
				let ch1_ptr = *data_ptr.offset(1) as *const i32;
				let ch0 = from_raw_parts(ch0_ptr, nb_samples as usize);
				let ch1 = from_raw_parts(ch1_ptr, nb_samples as usize);
				let mut i = 0usize;
				let mut right = false;
				let words = std::iter::from_fn(|| {
					if i >= nb_samples as usize
					{
						return None;
					}
					let w = if !right
					{
						right = true;
						((ch0[i] >> 16) as i16) as u16
					}
					else
					{
						right = false;
						let w = ((ch1[i] >> 16) as i16) as u16;
						i = i.saturating_add(1);
						w
					};
					Some(w)
				});
				dts.push_s16le_words(words);
				Ok(())
			}
			_ => Err(format!("DTS-in-PCM 不支持的采样格式: {}", format)),
		}
	}

	unsafe fn decode_next_dts_in_pcm(&mut self, funcs: &AvFunctions) -> Result<Vec<f64>, String> {
		loop
		{
			let out = match self.dts_in_pcm.as_mut()
			{
				Some(dts) => dts.try_decode()?,
				None => return Err("DTS-in-PCM 解码器未初始化".to_string()),
			};
			if let Some(samples) = out
			{
				return Ok(samples);
			}

			let got = self.receive_next_frame(funcs)?;
			if !got
			{
				if let Some(ref mut dts) = self.dts_in_pcm
				{
					dts.set_input_eof();
				}
				continue;
			}

			self.feed_dts_in_pcm_from_frame()?;
			(funcs.av_frame_unref)(self.frame);
		}
	}

	unsafe fn decode_next(&mut self) -> Result<Vec<f64>, String> {
		let funcs = AV_FUNCS
			.as_ref()
			.ok_or("FFmpeg DLL 未加载")?;

		if self.dts_in_pcm.is_some()
		{
			return self.decode_next_dts_in_pcm(funcs);
		}

		loop
		{
			let got = self.receive_next_frame(funcs)?;
			if !got
			{
				return Ok(Vec::new());
			}

			let nb_samples = (*self.frame).nb_samples;
			if nb_samples <= 0
			{
				(funcs.av_frame_unref)(self.frame);
				continue;
			}

			let format = (*self.frame).format;
			// data 已经在 struct 中定义为数组
			let data_ptr = if !(*self.frame).extended_data.is_null()
			{
				(*self.frame).extended_data // 通常 data[0] 等于 extended_data[0]
			}
			else
			{
				(*self.frame).data.as_ptr() as *mut *mut u8
			};

			let channels = self.channels as i32;
			if channels <= 0
			{
				(funcs.av_frame_unref)(self.frame);
				return Err("FFmpeg 解码帧声道数无效".to_string());
			}

			if !self.pcm_probe_checked && self.channels == 2 && self.bits_per_sample == 16
			{
				let max_words = ((nb_samples * channels) as usize).min(65536);
				let mut hit: Option<usize> = None;

				match format
				{
					1 =>
					{
						// AV_SAMPLE_FMT_S16 (packed)
						if !data_ptr.is_null() && !(*data_ptr.offset(0)).is_null()
						{
							let ptr = *data_ptr.offset(0) as *const u16;
							let words = from_raw_parts(ptr, (nb_samples * channels) as usize);
							hit = pcm_probe_dts_in_s16le_words(words.iter().take(max_words).copied());
						}
					}
					6 =>
					{
						// AV_SAMPLE_FMT_S16P (planar)
						if !data_ptr.is_null() && !(*data_ptr.offset(0)).is_null() && !(*data_ptr.offset(1)).is_null()
						{
							let ch0_ptr = *data_ptr.offset(0) as *const u16;
							let ch1_ptr = *data_ptr.offset(1) as *const u16;
							let ch0 = from_raw_parts(ch0_ptr, nb_samples as usize);
							let ch1 = from_raw_parts(ch1_ptr, nb_samples as usize);
							let mut i = 0usize;
							let mut right = false;
							let words = std::iter::from_fn(|| {
								if i >= nb_samples as usize
								{
									return None;
								}
								let w = if !right
								{
									right = true;
									ch0[i]
								}
								else
								{
									right = false;
									let w = ch1[i];
									i = i.saturating_add(1);
									w
								};
								Some(w)
							});
							hit = pcm_probe_dts_in_s16le_words(words.take(max_words));
						}
					}
					2 =>
					{
						// AV_SAMPLE_FMT_S32 (packed)
						if !data_ptr.is_null() && !(*data_ptr.offset(0)).is_null()
						{
							let ptr = *data_ptr.offset(0) as *const i32;
							let words32 = from_raw_parts(ptr, (nb_samples * channels) as usize);
							hit = pcm_probe_dts_in_s16le_words(
								words32
									.iter()
									.take(max_words)
									.map(|&s| ((s >> 16) as i16) as u16),
							);
						}
					}
					7 =>
					{
						// AV_SAMPLE_FMT_S32P (planar)
						if !data_ptr.is_null() && !(*data_ptr.offset(0)).is_null() && !(*data_ptr.offset(1)).is_null()
						{
							let ch0_ptr = *data_ptr.offset(0) as *const i32;
							let ch1_ptr = *data_ptr.offset(1) as *const i32;
							let ch0 = from_raw_parts(ch0_ptr, nb_samples as usize);
							let ch1 = from_raw_parts(ch1_ptr, nb_samples as usize);
							let mut i = 0usize;
							let mut right = false;
							let words = std::iter::from_fn(|| {
								if i >= nb_samples as usize
								{
									return None;
								}
								let w = if !right
								{
									right = true;
									((ch0[i] >> 16) as i16) as u16
								}
								else
								{
									right = false;
									let w = ((ch1[i] >> 16) as i16) as u16;
									i = i.saturating_add(1);
									w
								};
								Some(w)
							});
							hit = pcm_probe_dts_in_s16le_words(words.take(max_words));
						}
					}
					_ => {}
				}

				self.pcm_probe_checked = true;
				if let Some(offset_words) = hit
				{
					eprintln!("[dec] PCM 探测: 检测到 DTS-in-PCM (offset_words={})，启用二次解码", offset_words);
					self.dts_in_pcm = Some(FfmpegDtsInPcmDecoder::new()?);
					self.feed_dts_in_pcm_from_frame()?;
					(funcs.av_frame_unref)(self.frame);
					return self.decode_next_dts_in_pcm(funcs);
				}
			}

			let mut samples = Vec::with_capacity((nb_samples * channels) as usize);

			// 注意: extended_data 是 *mut *mut u8

			let fill_res: Result<(), String> = match format
			{
					0 =>
					{
						// AV_SAMPLE_FMT_U8 (packed)
						if data_ptr.is_null() || (*data_ptr.offset(0)).is_null()
						{
							return Err("FFmpeg 解码帧数据为空".to_string());
						}
						let ptr = *data_ptr.offset(0) as *const u8;
						for i in 0..(nb_samples * channels)
						{
							let val = *ptr.offset(i as isize);
							samples.push((val as f64 - 128.0) / 128.0);
						}
						Ok(())
					}
					1 =>
					{
						// AV_SAMPLE_FMT_S16 (packed)
						if data_ptr.is_null() || (*data_ptr.offset(0)).is_null()
						{
							return Err("FFmpeg 解码帧数据为空".to_string());
						}
						let ptr = *data_ptr.offset(0) as *const i16;
						for i in 0..(nb_samples * channels)
						{
							let val = *ptr.offset(i as isize);
							samples.push(val as f64 / 32768.0);
						}
						Ok(())
					}
					2 =>
					{
						// AV_SAMPLE_FMT_S32 (packed)
						if data_ptr.is_null() || (*data_ptr.offset(0)).is_null()
						{
							return Err("FFmpeg 解码帧数据为空".to_string());
						}
						let ptr = *data_ptr.offset(0) as *const i32;
						for i in 0..(nb_samples * channels)
						{
							let val = *ptr.offset(i as isize);
							samples.push(val as f64 / 2147483648.0);
						}
						Ok(())
					}
					6 =>
					{
						// AV_SAMPLE_FMT_S16P (planar)
						if (*self.frame).extended_data.is_null() && channels > 8
						{
							return Err("FFmpeg planar 输出声道数>8 但 extended_data 为空".to_string());
						}
						for i in 0..nb_samples
						{
							for c in 0..channels
							{
								let ch_data = *data_ptr.offset(c as isize);
								if ch_data.is_null()
								{
									return Err("FFmpeg 解码帧数据为空".to_string());
								}
								let val = *(ch_data as *const i16).offset(i as isize);
								samples.push(val as f64 / 32768.0);
							}
						}
						Ok(())
					}
					3 =>
					{
						// AV_SAMPLE_FMT_FLT (packed float)
						if data_ptr.is_null() || (*data_ptr.offset(0)).is_null()
						{
							return Err("FFmpeg 解码帧数据为空".to_string());
						}
						let ptr = *data_ptr.offset(0) as *const f32;
						for i in 0..(nb_samples * channels)
						{
							let val = *ptr.offset(i as isize);
							samples.push(val as f64);
						}
						Ok(())
					}
					4 =>
					{
						// AV_SAMPLE_FMT_DBL (packed double)
						if data_ptr.is_null() || (*data_ptr.offset(0)).is_null()
						{
							return Err("FFmpeg 解码帧数据为空".to_string());
						}
						let ptr = *data_ptr.offset(0) as *const f64;
						for i in 0..(nb_samples * channels)
						{
							let val = *ptr.offset(i as isize);
							samples.push(val);
						}
						Ok(())
					}
					8 =>
					{
						// AV_SAMPLE_FMT_FLTP (planar float)
						if (*self.frame).extended_data.is_null() && channels > 8
						{
							return Err("FFmpeg planar 输出声道数>8 但 extended_data 为空".to_string());
						}
						for i in 0..nb_samples
						{
							for c in 0..channels
							{
								let ch_data = *data_ptr.offset(c as isize);
								if ch_data.is_null()
								{
									return Err("FFmpeg 解码帧数据为空".to_string());
								}
								let val = *(ch_data as *const f32).offset(i as isize);
								samples.push(val as f64);
							}
						}
						Ok(())
					}
					5 =>
					{
						// AV_SAMPLE_FMT_U8P (planar)
						if (*self.frame).extended_data.is_null() && channels > 8
						{
							return Err("FFmpeg planar 输出声道数>8 但 extended_data 为空".to_string());
						}
						for i in 0..nb_samples
						{
							for c in 0..channels
							{
								let ch_data = *data_ptr.offset(c as isize);
								if ch_data.is_null()
								{
									return Err("FFmpeg 解码帧数据为空".to_string());
								}
								let val = *(ch_data as *const u8).offset(i as isize);
								samples.push((val as f64 - 128.0) / 128.0);
							}
						}
						Ok(())
					}
					7 =>
					{
						// AV_SAMPLE_FMT_S32P (planar)
						if (*self.frame).extended_data.is_null() && channels > 8
						{
							return Err("FFmpeg planar 输出声道数>8 但 extended_data 为空".to_string());
						}
						for i in 0..nb_samples
						{
							for c in 0..channels
							{
								let ch_data = *data_ptr.offset(c as isize);
								if ch_data.is_null()
								{
									return Err("FFmpeg 解码帧数据为空".to_string());
								}
								let val = *(ch_data as *const i32).offset(i as isize);
								samples.push(val as f64 / 2147483648.0);
							}
						}
						Ok(())
					}
					9 =>
					{
						// AV_SAMPLE_FMT_DBLP (planar)
						if (*self.frame).extended_data.is_null() && channels > 8
						{
							return Err("FFmpeg planar 输出声道数>8 但 extended_data 为空".to_string());
						}
						for i in 0..nb_samples
						{
							for c in 0..channels
							{
								let ch_data = *data_ptr.offset(c as isize);
								if ch_data.is_null()
								{
									return Err("FFmpeg 解码帧数据为空".to_string());
								}
								let val = *(ch_data as *const f64).offset(i as isize);
								samples.push(val);
							}
						}
						Ok(())
					}
					10 =>
					{
						// AV_SAMPLE_FMT_S64 (packed)
						if data_ptr.is_null() || (*data_ptr.offset(0)).is_null()
						{
							return Err("FFmpeg 解码帧数据为空".to_string());
						}
						let ptr = *data_ptr.offset(0) as *const i64;
						for i in 0..(nb_samples * channels)
						{
							let val = *ptr.offset(i as isize);
							samples.push(val as f64 / 9223372036854775808.0);
						}
						Ok(())
					}
					11 =>
					{
						// AV_SAMPLE_FMT_S64P (planar)
						if (*self.frame).extended_data.is_null() && channels > 8
						{
							return Err("FFmpeg planar 输出声道数>8 但 extended_data 为空".to_string());
						}
						for i in 0..nb_samples
						{
							for c in 0..channels
							{
								let ch_data = *data_ptr.offset(c as isize);
								if ch_data.is_null()
								{
									return Err("FFmpeg 解码帧数据为空".to_string());
								}
								let val = *(ch_data as *const i64).offset(i as isize);
								samples.push(val as f64 / 9223372036854775808.0);
							}
						}
						Ok(())
					}
					_ => Err(format!("FFmpeg 不支持的采样格式: {}", format)),
			};

			(funcs.av_frame_unref)(self.frame);

			match fill_res
			{
				Ok(_) => return Ok(samples),
				Err(e) => return Err(e),
			}
		}
	}

	unsafe fn seek(&mut self, ms: i64) -> Result<(), String> {
		let funcs = AV_FUNCS
			.as_ref()
			.ok_or("FFmpeg DLL 未加载")?;

		const AVSEEK_FLAG_BACKWARD: i32 = 1;
		const AVSEEK_FLAG_ANY: i32 = 4;

		let ms = ms.max(0);
		let flags = AVSEEK_FLAG_BACKWARD | AVSEEK_FLAG_ANY;

		(funcs.av_packet_unref)(self.pkt);
		(funcs.av_frame_unref)(self.frame);

		let mut ok = false;

		// 优先按音频流 time_base seek，并补偿该文件的首包时间戳偏移
		if let Some(rel_ts) = Self::ms_to_stream_ts(ms, self.stream_time_base)
		{
			let timestamp = self
				.stream_first_ts
				.saturating_add(rel_ts);

			if (funcs.av_seek_frame)(self.ctx, self.stream_idx, timestamp, flags) >= 0
			{
				ok = true;
			}
		}

		// fallback: AV_TIME_BASE (microseconds) seek (may still work for some formats)
		if !ok
		{
			let timestamp = ms.saturating_mul(1000);
			if (funcs.av_seek_frame)(self.ctx, -1, timestamp, flags) >= 0
			{
				ok = true;
			}
		}

		if !ok
		{
			return Err("av_seek_frame 失败".to_string());
		}

		(funcs.avcodec_flush_buffers)(self.codec_ctx);
		if let Some(ref mut dts) = self.dts_in_pcm
		{
			dts.reset()?;
		}
		Ok(())
	}
}

impl Drop for FFmpegDecoder {
	fn drop(&mut self) {
		unsafe {
			if let Some(funcs) = AV_FUNCS.as_ref()
			{
				let mut codec_ctx = self.codec_ctx;
				let mut ctx = self.ctx;
				let mut pkt = self.pkt;
				let mut frame = self.frame;
				(funcs.avcodec_free_context)(&mut codec_ctx);
				(funcs.avformat_close_input)(&mut ctx);
				(funcs.av_packet_free)(&mut pkt);
				(funcs.av_frame_free)(&mut frame);
			}
		}
	}
}

// === audio_dec trait 实现 ===

unsafe fn create_ffmpeg_decoder(path: &str) -> Result<Box<dyn audio_dec>, String> {
	FFmpegDecoder::new(path).map(|d| Box::new(d) as Box<dyn audio_dec>)
}

impl audio_dec for FFmpegDecoder {
	fn info(&self) -> DecoderInfo {
		DecoderInfo {
			sample_rate: self.sample_rate,
			channels: self.channels,
			bits_per_sample: self.bits_per_sample,
			duration_ms: Some(self.duration_ms),
			codec_name: "ffmpeg",
		}
	}

	fn decode_block(&mut self, _max_frames: usize) -> Result<Vec<f64>, String> {
		unsafe { self.decode_next() }
	}

	fn seek_ms(&mut self, ms: i64) -> Result<(), String> {
		unsafe { self.seek(ms) }
	}
}

// src\decode.rs
// 解码线程 - 负责文件读取、解码、重采样，并写入 RingBuffer
// 这是生产者线程，可以预读数据，不受实时约束
// 使用插件化解码器架构

enum DecodeCommand {
	Start { path: String, start_ms: u64, output_sample_rate: usize, channels: u16 },
}

struct seq_vec_adapter<'a> {
	buf: &'a [Vec<f64>],
	channels: usize,
	frames: usize,
}

struct interleaved_mut_adapter<'a> {
	buf: &'a mut [f64],
	channels: usize,
	frames: usize,
}

impl<'a> interleaved_mut_adapter<'a> {
	#[inline]
	fn calc_index(&self, channel: usize, frame: usize) -> usize {
		frame * self.channels + channel
	}
}

unsafe impl<'a> rubato::audioadapter::Adapter<'a, f64> for seq_vec_adapter<'a> {
	unsafe fn read_sample_unchecked(&self, channel: usize, frame: usize) -> f64 {
		*self
			.buf
			.get_unchecked(channel)
			.get_unchecked(frame)
	}

	fn channels(&self) -> usize {
		self.channels
	}

	fn frames(&self) -> usize {
		self.frames
	}

	fn copy_from_channel_to_slice(&self, channel: usize, skip: usize, slice: &mut [f64]) -> usize {
		if channel >= self.channels || skip >= self.frames
		{
			return 0;
		}

		let frames_left = self.frames - skip;
		let frames_to_write = if frames_left < slice.len()
		{
			frames_left
		}
		else
		{
			slice.len()
		};

		slice[..frames_to_write]
			.copy_from_slice(&self.buf[channel][skip..skip + frames_to_write]);
		frames_to_write
	}
}

unsafe impl<'a> rubato::audioadapter::Adapter<'a, f64> for interleaved_mut_adapter<'a> {
	unsafe fn read_sample_unchecked(&self, channel: usize, frame: usize) -> f64 {
		let index = self.calc_index(channel, frame);
		*self.buf.get_unchecked(index)
	}

	fn channels(&self) -> usize {
		self.channels
	}

	fn frames(&self) -> usize {
		self.frames
	}
}

unsafe impl<'a> rubato::audioadapter::AdapterMut<'a, f64> for interleaved_mut_adapter<'a> {
	unsafe fn write_sample_unchecked(&mut self, channel: usize, frame: usize, value: &f64) -> bool {
		let index = self.calc_index(channel, frame);
		*self.buf.get_unchecked_mut(index) = *value;
		false
	}
}

/// 解码线程入口（常驻）
/// producer: RingBuffer 的生产者端（线程启动时持有）
unsafe fn decode_thread(mut producer: ringbuf::HeapProd<f64>, rx: mpsc::Receiver<DecodeCommand>) {
	// 初始：空闲
	SetEvent(g_ev_dec_idle);

	loop
	{
		let cmd = match rx.recv()
		{
			Ok(c) => c,
			Err(_) =>
			{
				// 所有 Sender 已释放：避免其他线程永远等待
				SetEvent(g_ev_dec_idle);
				return;
			}
		};

		match cmd
		{
			DecodeCommand::Start { path, start_ms, output_sample_rate, channels } =>
			{
				// 标记为忙（manual-reset）
				ResetEvent(g_ev_dec_idle);

				// 任务级状态初始化
				g_seek_to_ms.store(-1, Ordering::SeqCst);
				g_seek_just.store(false, Ordering::SeqCst);

				decode_task(path, start_ms, &mut producer, output_sample_rate, channels);

				// 任务结束：标记空闲
				SetEvent(g_ev_dec_idle);
			}
		}
	}
}

/// 解码一个任务（单曲生命周期），直到收到 g_dec_stop 或进程退出
unsafe fn decode_task(path: String, start_ms: u64, producer: &mut ringbuf::HeapProd<f64>, output_sample_rate: usize, channels: u16) {
	eprintln!("[dec] 启动: path={}", path);

	// ========== 使用插件化解码器 ==========
	let mut decoder: Box<dyn audio_dec> = match create_decoder(&path)
	{
		Ok(d) => d,
		Err(e) =>
		{
			eprintln!("[dec] 创建解码器失败: {}", e);
			// 推入 EOS 标记，避免播放线程在缓冲耗尽后无限输出静音
			for _ in 0..channels as usize
			{
				while producer.try_push(EOS_MARKER).is_err()
				{
					if g_dec_stop.load(Ordering::SeqCst)
					{
						eprintln!("[dec] 退出");
						return;
					}
					WaitForSingleObject(g_ev_ring_space, 10);
				}
			}
			// 保持线程存活，等待 stop/seek，让生命周期与一首歌绑定
			while !g_dec_stop.load(Ordering::SeqCst)
			{
				WaitForSingleObject(g_ev_dec_wakeup, 0xFFFFFFFF);
			}
			eprintln!("[dec] 退出");
			return;
		}
	};

	let info = decoder.info();
	if info.bits_per_sample == 0
	{
		eprintln!("[dec] 解码器: {} ({}Hz/?bit/{}ch)", info.codec_name, info.sample_rate, info.channels);
	}
	else
	{
		eprintln!("[dec] 解码器: {} ({}Hz/{}bit/{}ch)", info.codec_name, info.sample_rate, info.bits_per_sample, info.channels);
	}

	// 如果有起始位置，seek 到该位置
	if start_ms > 0
	{
		if let Err(e) = decoder.seek_ms(start_ms as i64)
		{
			eprintln!("[dec] Seek 失败: {}", e);
		}
		else
		{
			eprintln!("[dec] 从 {}ms 恢复解码", start_ms);
		}
	}

	// ========== 创建重采样器（如果需要）==========
	let needs_resample = info.sample_rate as usize != output_sample_rate;
	let chunk_size = 1024usize;

	let mut resampler: Option<Async<f64>> = if needs_resample
	{
		let sinc_params = SincInterpolationParameters {
			sinc_len: 128,
			f_cutoff: 0.95,
			interpolation: SincInterpolationType::Cubic,
			oversampling_factor: 128,
			window: WindowFunction::Blackman,
		};
		match Async::<f64>::new_sinc(
			output_sample_rate as f64 / info.sample_rate as f64,
			2.0,
			&sinc_params,
			chunk_size,
			info.channels as usize,
			FixedAsync::Input,
		)
		{
			Ok(r) =>
			{
				eprintln!("[dec] 创建重采样器: {}Hz -> {}Hz", info.sample_rate, output_sample_rate);
				Some(r)
			}
			Err(e) =>
			{
				eprintln!("[dec] 重采样器创建失败: {:?}", e);
				None
			}
		}
	}
	else
	{
		None
	};

	let actual_input_frames = resampler
		.as_ref()
		.map(|r| r.input_frames_next())
		.unwrap_or(chunk_size);
	let output_frames_max = resampler
		.as_ref()
		.map(|r| r.output_frames_max())
		.unwrap_or(0);

	// 重采样输入缓冲（按声道分离）
	let mut resample_input: Vec<Vec<f64>> = (0..info.channels as usize)
		.map(|_| Vec::with_capacity(actual_input_frames * 4))
		.collect();
	let mut resample_offset: usize = 0;
	let mut resample_output: Vec<f64> = if output_frames_max > 0
	{
		vec![0.0; output_frames_max * info.channels as usize]
	}
	else
	{
		Vec::new()
	};

	eprintln!("[dec] 开始解码循环");

	// ========== 统一解码循环 ==========
	let mut at_eof = false;
	let mut eos_marker_pushed = false;
	loop
	{
		// 1. 检查停止请求
		if g_dec_stop.load(Ordering::SeqCst)
		{
			eprintln!("[dec] 收到停止请求");
			break;
		}

		// 2. 检查 Seek 请求
		let seek_ms = g_seek_to_ms.swap(-1, Ordering::SeqCst);
		if seek_ms >= 0
		{
			// Seek 通常伴随 playback 侧清空 RingBuffer；若之前已推入 EOS 标记，需要允许重新推入
			eos_marker_pushed = false;
			match decoder.seek_ms(seek_ms)
			{
				Ok(_) =>
				{
					// 清空重采样缓冲
					for ch in resample_input.iter_mut()
					{
						ch.clear();
					}
					resample_offset = 0;
					// Seek 属于流的不连续点：重置重采样器内部状态，避免把上一段的滤波尾巴“带到”新位置产生瞬态咔嚓
					if let Some(ref mut rs) = resampler
					{
						rs.reset();
					}
					g_seek_just.store(true, Ordering::SeqCst);
					at_eof = false;
					eprintln!("[dec] Seek-to {}ms", seek_ms);
				}
				Err(e) =>
				{
					eprintln!("[dec] Seek 失败: {}", e);
				}
			}
		}

		// 2.1 EOF 状态：保持线程存活，等待 stop/seek
		if at_eof
		{
			if !eos_marker_pushed
			{
				eos_marker_pushed = true;
				for _ in 0..channels as usize
				{
					while producer.try_push(EOS_MARKER).is_err()
					{
						if g_dec_stop.load(Ordering::SeqCst)
						{
							eprintln!("[dec] 退出");
							return;
						}
						WaitForSingleObject(g_ev_ring_space, 10);
					}
				}
			}
			// 等待唤醒事件（Seek 或 Stop 请求时触发）
			WaitForSingleObject(g_ev_dec_wakeup, 0xFFFFFFFF);
			continue;
		}

		// 3. 暂停时等待恢复事件
		if WaitForSingleObject(g_ev_resume, 0) == 258
		{
			// 等待恢复或外部唤醒（stop/seek），避免暂停时 stop 卡死
			let hs = [g_ev_resume, g_ev_dec_wakeup];
			WaitForMultipleObjects(2, hs.as_ptr(), 0, 0xFFFFFFFF);
			continue;
		}

		// 4. 如果 RingBuffer 快满了，等待消费者通知
		while producer.vacant_len() < (output_sample_rate * info.channels as usize / 10) && !g_dec_stop.load(Ordering::SeqCst)
		{
			WaitForSingleObject(g_ev_ring_space, 50);
		}

		if g_dec_stop.load(Ordering::SeqCst)
		{
			break;
		}

		// 5. 解码一批样本
		match decoder.decode_block(4096)
		{
			Ok(samples) =>
			{
				if samples.is_empty()
				{
					eprintln!("[dec] EOF");
					at_eof = true;
					continue;
				}

				// 处理解码后的样本（重采样或直接输出）
				if let Some(ref mut rs) = resampler
				{
					// 分离声道
					let frames = samples.len() / info.channels as usize;
					for i in 0..frames
					{
						for ch in 0..info.channels as usize
						{
							let s = samples[i * info.channels as usize + ch];
							resample_input[ch].push(s);
						}
					}

					// 当累积足够样本时进行重采样
					let mut input_frames_needed = rs.input_frames_next();
					loop
					{
						let available_frames = resample_input
							.iter()
							.map(|ch| ch.len())
							.min()
							.unwrap_or(0);
						if available_frames
							.saturating_sub(resample_offset)
							< input_frames_needed
						{
							break;
						}

						let input_adapter = seq_vec_adapter {
							buf: &resample_input,
							channels: info.channels as usize,
							frames: available_frames,
						};

						let indexing = rubato::Indexing {
							input_offset: resample_offset,
							output_offset: 0,
							partial_len: None,
							active_channels_mask: None,
						};

						let out_frames = match {
							let mut output_adapter = interleaved_mut_adapter {
								buf: &mut resample_output,
								channels: info.channels as usize,
								frames: output_frames_max,
							};
							rs.process_into_buffer(&input_adapter, &mut output_adapter, Some(&indexing))
						}
						{
							Ok((in_frames, out_frames)) =>
							{
								resample_offset += in_frames;
								out_frames
							}
							Err(e) =>
							{
								eprintln!("[dec] 重采样失败: {:?}", e);
								0
							}
						};

						let out_len = out_frames * info.channels as usize;
						let mut pos = 0usize;
						while pos < out_len
						{
							let n = producer.push_slice(&resample_output[pos..out_len]);
							pos += n;
							if pos < out_len
							{
								if g_dec_stop.load(Ordering::SeqCst)
								{
									return;
								}
								WaitForSingleObject(g_ev_ring_space, 10);
							}
						}

						// Avoid O(n) memmoves on every iteration; compact in larger chunks.
						if resample_offset > 0 && resample_offset >= actual_input_frames * 4
						{
							for ch in resample_input.iter_mut()
							{
								ch.drain(..resample_offset);
							}
							resample_offset = 0;
						}

						input_frames_needed = rs.input_frames_next();
					}
				}
				else
				{
					// 不需要重采样，直接推入 RingBuffer
					let mut pos = 0usize;
					while pos < samples.len()
					{
						let n = producer.push_slice(&samples[pos..]);
						pos += n;
						if pos < samples.len()
						{
							if g_dec_stop.load(Ordering::SeqCst)
							{
								return;
							}
							WaitForSingleObject(g_ev_ring_space, 10);
						}
					}
				}
			}
			Err(e) =>
			{
				eprintln!("[dec] 解码错误: {}", e);
				// 解码错误：把它视为 EOF，推入 EOS 标记并等待 stop/seek
				at_eof = true;
				continue;
			}
		}
	}

	eprintln!("[dec] 退出");
}

// src\decoder.rs
// 插件化解码器架构
// 统一的解码器接口，支持按扩展名注册解码器

/// 解码器信息
struct DecoderInfo {
	sample_rate: u32,
	channels: u16,
	bits_per_sample: u16,
	duration_ms: Option<u64>,
	codec_name: &'static str,
}

/// 统一的解码器 trait
trait audio_dec {
	/// 获取音频信息
	fn info(&self) -> DecoderInfo;

	/// 解码一批样本，返回 interleaved f64
	/// 返回空 Vec 表示 EOF
	fn decode_block(&mut self, max_frames: usize) -> Result<Vec<f64>, String>;

	/// Seek 到指定毫秒位置
	fn seek_ms(&mut self, ms: i64) -> Result<(), String>;
}

/// 解码器工厂函数类型 (unsafe)
type DecoderFactory = unsafe fn(&str) -> Result<Box<dyn audio_dec>, String>;

// ==================== PCM 探测 (bitstream-in-PCM) ====================

fn pcm_probe_dts_sync_s16le_word_pair(a: u16, b: u16) -> bool {
	matches!((a, b), (0xFE7F, 0x0180) | (0x7FFE, 0x8001) | (0xFF1F, 0x00E8) | (0x1FFF, 0xE800))
}

fn pcm_probe_dts_in_s16le_words<I>(words: I) -> Option<usize>
where
	I: IntoIterator<Item = u16>,
{
	let mut prev: Option<u16> = None;
	let mut idx: usize = 0;
	for w in words
	{
		if let Some(p) = prev
		{
			if pcm_probe_dts_sync_s16le_word_pair(p, w)
			{
				return Some(idx.saturating_sub(1));
			}
		}
		prev = Some(w);
		idx = idx.saturating_add(1);
	}
	None
}

fn symphonia_decode_error_is_recoverable(e: &symphonia::core::errors::Error) -> bool {
	matches!(e, symphonia::core::errors::Error::DecodeError(_) | symphonia::core::errors::Error::IoError(_))
}

fn symphonia_probe_decoded_output_spec(path: &str) -> Result<Option<(u32, u16)>, String> {
	let file = File::open(path).map_err(|e| format!("无法打开文件: {:?}", e))?;
	let mss = MediaSourceStream::new(Box::new(file), Default::default());

	let mut hint = Hint::new();
	if let Some(ext) = Path::new(path).extension()
	{
		if let Some(ext_str) = ext.to_str()
		{
			hint.with_extension(ext_str);
		}
	}

	let mut format = symphonia::default::get_probe()
		.probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
		.map_err(|e| format!("探测格式失败: {:?}", e))?;

	let track = format
		.default_track(TrackType::Audio)
		.ok_or("无音轨")?
		.clone();
	let track_id = track.id;

	let codec_params = match &track.codec_params
	{
		Some(CodecParameters::Audio(p)) => p.clone(),
		_ => return Ok(None),
	};

	let mut decoder = symphonia::default::get_codecs()
		.make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
		.map_err(|e| format!("解码器创建失败: {:?}", e))?;

	for _ in 0..32
	{
		match format.next_packet()
		{
			Ok(Some(packet)) =>
			{
				if packet.track_id != track_id
				{
					continue;
				}

				match decoder.decode(&packet)
				{
					Ok(decoded) => return Ok(Some((decoded.spec().rate(), decoded.spec().channels().count() as u16))),
					Err(e) =>
					{
						if symphonia_decode_error_is_recoverable(&e)
						{
							continue;
						}
						return Err(format!("解码器探测失败: {:?}", e));
					}
				}
			}
			Ok(None) => break,
			Err(_) => break,
		}
	}

	Ok(None)
}

/// 解码器注册项
struct DecoderEntry {
	extensions: &'static [&'static str],
	factory: DecoderFactory,
	name: &'static str,
}

/// 解码器注册表
static DECODER_REGISTRY: LazyLock<Vec<DecoderEntry>> = LazyLock::new(|| {
	vec![
		DecoderEntry { extensions: &["ape"], factory: create_mac_decoder, name: "Monkey's Audio" },
		DecoderEntry {
			extensions: &["mp3", "flac", "wav", "ogg", "m4a", "aac", "aiff"],
			factory: create_symphonia_decoder,
			name: "Symphonia",
		},
		// FFmpeg 解码器 - 支持 WMA 等 Symphonia 不支持的格式
		DecoderEntry { extensions: &["wma", "wmv", "asf"], factory: create_ffmpeg_decoder, name: "FFmpeg" },
	]
});

/// 根据路径创建解码器
unsafe fn create_decoder(path: &str) -> Result<Box<dyn audio_dec>, String> {
	let ext = Path::new(path)
		.extension()
		.and_then(|e| e.to_str())
		.map(|e| e.to_ascii_lowercase())
		.unwrap_or_default();

	let mut errors: Vec<String> = Vec::new();
	let mut tried_any = false;
	let mut matched = false;
	let mut ffmpeg_tried = false;

	for entry in DECODER_REGISTRY.iter()
	{
		if !entry
			.extensions
			.iter()
			.any(|&e| e == ext)
		{
			continue;
		}

		matched = true;
		if entry.name == "FFmpeg"
		{
			ffmpeg_tried = true;
		}

		tried_any = true;
		// eprintln!("[decoder] 尝试 {} 解码器", entry.name);
		match (entry.factory)(path)
		{
			Ok(dec) =>
			{
				eprintln!("[dec] 使用 {} 解码器", entry.name);
				return Ok(dec);
			}
			Err(e) =>
			{
				eprintln!("[dec] {} 初始化失败: {}", entry.name, e);
				errors.push(format!("{}: {}", entry.name, e));
			}
		}
	}

	// Symphonia 等解码器失败时，尝试 FFmpeg 作为兜底（即便扩展名未注册）
	if !ffmpeg_tried
	{
		tried_any = true;
		//eprintln!("[decoder] 尝试 FFmpeg 解码器 (fallback)");
		match create_ffmpeg_decoder(path)
		{
			Ok(dec) =>
			{
				eprintln!("[dec] 使用 FFmpeg 解码器");
				return Ok(dec);
			}
			Err(e) =>
			{
				eprintln!("[dec] FFmpeg fallback 失败: {}", e);
				errors.push(format!("FFmpeg: {}", e));
			}
		}
	}

	if tried_any
	{
		Err(format!("解码器初始化失败: {}\n{}", path, errors.join("\n")))
	}
	else
	{
		let ext = if ext.is_empty() { "<none>" } else { ext.as_str() };
		Err(format!("不支持的格式: {}", ext))
	}
}

// ==================== MacDecoder 适配 ====================

unsafe fn create_mac_decoder(path: &str) -> Result<Box<dyn audio_dec>, String> {
	MacDecoder::new(path).map(|d| Box::new(d) as Box<dyn audio_dec>)
}

impl audio_dec for MacDecoder {
	fn info(&self) -> DecoderInfo {
		DecoderInfo {
			sample_rate: self.sample_rate,
			channels: self.channels,
			bits_per_sample: self.bits_per_sample,
			duration_ms: Some(self.duration_ms),
			codec_name: "ape",
		}
	}

	fn decode_block(&mut self, max_frames: usize) -> Result<Vec<f64>, String> {
		unsafe { self.decode_blocks(max_frames) }
	}

	fn seek_ms(&mut self, ms: i64) -> Result<(), String> {
		// 将毫秒转换为 block 偏移: block = ms * sample_rate / 1000
		let block_offset = (ms as i64 * self.sample_rate as i64) / 1000;
		unsafe { self.seek_blocks(block_offset) }
	}
}

// ==================== SymphoniaDecoder 适配 ====================

struct SymphoniaDecoder {
	format: Box<dyn FormatReader>,
	decoder: Box<dyn AudioDecoder>,
	track_id: u32,
	time_base: TimeBase,
	stream_first_ts: Timestamp,
	sample_rate: u32,
	channels: u16,
	bits_per_sample: u16,
	codec_name: &'static str,
	duration_ms: Option<u64>,
	pcm_probe_checked: bool,
	dts_in_pcm: Option<FfmpegDtsInPcmDecoder>,
}

unsafe fn create_symphonia_decoder(path: &str) -> Result<Box<dyn audio_dec>, String> {
	let file = File::open(path).map_err(|e| format!("无法打开文件: {:?}", e))?;
	let mss = MediaSourceStream::new(Box::new(file), Default::default());

	let mut hint = Hint::new();
	if let Some(ext) = Path::new(path).extension()
	{
		if let Some(ext_str) = ext.to_str()
		{
			hint.with_extension(ext_str);
		}
	}

	let mut format = symphonia::default::get_probe()
		.probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
		.map_err(|e| format!("探测格式失败: {:?}", e))?;

	let track = format
		.default_track(TrackType::Audio)
		.ok_or("无音轨")?
		.clone();
	let track_id = track.id;

	let codec_params = match &track.codec_params
	{
		Some(CodecParameters::Audio(p)) => p.clone(),
		_ => return Err("无可用音轨参数".to_string()),
	};

	let mut sample_rate = codec_params
		.sample_rate
		.unwrap_or(44100);
	let mut channels = codec_params
		.channels
		.as_ref()
		.map(|c| c.count())
		.unwrap_or(2) as u16;
	let bits_per_sample = codec_params
		.bits_per_sample
		.unwrap_or(0) as u16;
	let codec_name = symphonia::default::get_codecs()
		.get_audio_decoder(codec_params.codec)
		.map(|c| c.codec.info.short_name)
		.unwrap_or("unknown");

	let time_base = track
		.time_base
		.or_else(|| TimeBase::try_from_recip(sample_rate))
		.unwrap_or_default();

	let ts_to_ms = |ts: Timestamp| -> u64 {
		let ms = time_base
			.calc_time_saturating(ts)
			.as_millis()
			.clamp(0, u64::MAX as i128);
		ms as u64
	};

	// 一些被 `ffmpeg -c copy` 分割出来的无损文件，会保留原始流的时间戳/样本序号偏移：
	// - WavPack 的 block_idx（样本序号）可能不是从 0 开始
	// - FLAC 的 frame sample number 也可能不是从 0 开始
	// 这会导致“曲内相对时间(从0ms起)”去 seek 时，看起来总是回到曲首。
	// 方案：记录首包时间戳作为偏移，seek 时补上偏移。
	let mut stream_first_ts = track.start_ts;
	let mut did_scan_to_eof = false;
	let mut scan_end_ts: Option<Timestamp> = None;
	let mut format_needs_rewind = false;

	// 探测首包 ts（并记录首包 end_ts，供后续扫描初始化）
	if stream_first_ts.is_zero()
	{
		let mut first_end_ts: Option<Timestamp> = None;
		for _ in 0..256
		{
			match format.next_packet()
			{
				Ok(Some(packet)) =>
				{
					format_needs_rewind = true;
					if packet.track_id != track_id
					{
						continue;
					}
					stream_first_ts = packet.pts;
					first_end_ts = Some(packet.pts.saturating_add(packet.dur));
					break;
				}
				Ok(None) => break,
				Err(_) => break,
			}
		}

		scan_end_ts = first_end_ts;
	}

	// 负数起始 ts 常见于 MP3 encoder delay，不是分割残留偏移；播放器进度仍以 0ms 为曲首。
	if stream_first_ts.is_negative()
	{
		stream_first_ts = Timestamp::ZERO;
		scan_end_ts = None;
	}

	// 对 “正数起始 ts 的文件” 做一次扫描，修正 duration：
	// 有些分割出来的 FLAC/WV 文件会保留原始样本序号起点，但 StreamInfo 的 total_samples 没被重写，
	// 直接用 n_frames 计算 duration 会严重偏大，进而允许 seek 到根本不存在的位置，最后表现为 seek EOF。
	if stream_first_ts.is_positive()
	{
		let mut max_end_ts = scan_end_ts.unwrap_or(stream_first_ts);
		loop
		{
			match format.next_packet()
			{
				Ok(Some(packet)) =>
				{
					format_needs_rewind = true;
					if packet.track_id != track_id
					{
						continue;
					}
					let end_ts = packet.pts.saturating_add(packet.dur);
					if end_ts > max_end_ts
					{
						max_end_ts = end_ts;
					}
				}
				Ok(None) => break,
				Err(_) => break,
			}
		}
		did_scan_to_eof = true;
		scan_end_ts = Some(max_end_ts);
	}

	if format_needs_rewind
	{
		let seek_to = SeekTo::Timestamp { ts: stream_first_ts, track_id };
		if format
			.seek(SeekMode::Accurate, seek_to)
			.is_err()
		{
			let seek_to = SeekTo::Timestamp { ts: stream_first_ts, track_id };
			format
				.seek(SeekMode::Coarse, seek_to)
				.map_err(|e| format!("回到曲首失败: {:?}", e))?;
		}
	}

	// duration 计算：默认优先用容器给出的 duration；但遇到“非 0 起点”且扫描成功，则用扫描结果修正。
	let mut duration_ms: Option<u64> = track
		.duration
		.and_then(|d| Timestamp::try_from(d.get()).ok())
		.map(ts_to_ms)
		.or_else(|| {
			track
				.num_frames
				.and_then(|f| Timestamp::try_from(f).ok())
				.map(ts_to_ms)
		});

	if did_scan_to_eof && let Some(end_ts) = scan_end_ts
	{
		let delta = end_ts.saturating_delta(stream_first_ts);
		let dur_ts = Timestamp::try_from(delta.unsigned_abs()).unwrap_or(Timestamp::MAX);
		duration_ms = Some(ts_to_ms(dur_ts));
	}

	if stream_first_ts.is_positive()
	{
		let offset_ms = ts_to_ms(stream_first_ts);
		if did_scan_to_eof && let Some(d) = duration_ms
		{
			eprintln!("[Symphonia] stream_first_ts={} ({}ms), duration={}ms", stream_first_ts, offset_ms, d);
		}
		else
		{
			eprintln!("[Symphonia] stream_first_ts={} ({}ms)", stream_first_ts, offset_ms);
		}
	}

	let mut decoder = symphonia::default::get_codecs()
		.make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
		.map_err(|e| format!("解码器创建失败: {:?}", e))?;

	// 对 AAC(尤其 HE-AAC/SBR) 一类文件，容器/流参数里的 sample_rate 可能是“基础采样率”，
	// 但解码后的实际输出规格会是另一个采样率（常见：22050 -> 44100）。
	// 如果不修正，会导致后续重采样比率错误，最终听起来“又快又尖”。
	if codec_name == "aac" && let Ok(Some((rate, ch))) = symphonia_probe_decoded_output_spec(path)
	{
		if rate != sample_rate || ch != channels
		{
			eprintln!("[Symphonia] 输出规格修正: {}Hz/{}ch -> {}Hz/{}ch", sample_rate, channels, rate, ch);
			sample_rate = rate;
			channels = ch;
		}
	}

	decoder.reset();

	Ok(Box::new(SymphoniaDecoder {
		format,
		decoder,
		track_id,
		time_base,
		stream_first_ts,
		sample_rate,
		channels,
		bits_per_sample,
		codec_name,
		duration_ms,
		pcm_probe_checked: false,
		dts_in_pcm: None,
	}))
}

impl SymphoniaDecoder {
	fn feed_dts_in_pcm_from_decoded(dts: &mut FfmpegDtsInPcmDecoder, decoded: &GenericAudioBufferRef<'_>) -> Result<(), String> {
		let ch = decoded.spec().channels().count();
		if ch < 2
		{
			return Err("DTS-in-PCM 需要 2ch PCM".to_string());
		}

		let frames = decoded.frames();
		match decoded.clone()
		{
			GenericAudioBufferRef::S16(buf) =>
			{
				let Some((ch0, ch1)) = buf.plane_pair(0, 1)
				else
				{
					return Err("DTS-in-PCM 需要 2ch PCM".to_string());
				};
				let mut i = 0usize;
				let mut right = false;
				let words = std::iter::from_fn(|| {
					if i >= frames
					{
						return None;
					}
					let w = if !right
					{
						right = true;
						ch0[i] as u16
					}
					else
					{
						right = false;
						let w = ch1[i] as u16;
						i = i.saturating_add(1);
						w
					};
					Some(w)
				});
				dts.push_s16le_words(words);
				Ok(())
			}
			GenericAudioBufferRef::S32(buf) =>
			{
				let Some((ch0, ch1)) = buf.plane_pair(0, 1)
				else
				{
					return Err("DTS-in-PCM 需要 2ch PCM".to_string());
				};
				let mut i = 0usize;
				let mut right = false;
				let words = std::iter::from_fn(|| {
					if i >= frames
					{
						return None;
					}
					let w = if !right
					{
						right = true;
						((ch0[i] >> 16) as i16) as u16
					}
					else
					{
						right = false;
						let w = ((ch1[i] >> 16) as i16) as u16;
						i = i.saturating_add(1);
						w
					};
					Some(w)
				});
				dts.push_s16le_words(words);
				Ok(())
			}
			_ => Err("DTS-in-PCM 仅支持 s16/s32 PCM 输出".to_string()),
		}
	}

	fn decode_block_dts_in_pcm(&mut self) -> Result<Vec<f64>, String> {
		loop
		{
			let out = match self.dts_in_pcm.as_mut()
			{
				Some(dts) => unsafe { dts.try_decode() }?,
				None => return Err("DTS-in-PCM 解码器未初始化".to_string()),
			};

			match out
			{
				Some(samples) => return Ok(samples),
				None =>
				{}
			}

			match self.format.next_packet()
			{
				Ok(Some(packet)) =>
				{
					if packet.track_id != self.track_id
					{
						continue;
					}

					match self.decoder.decode(&packet)
					{
						Ok(decoded) =>
						{
							if let Some(ref mut dts) = self.dts_in_pcm
							{
								Self::feed_dts_in_pcm_from_decoded(dts, &decoded)?;
							}
						}
						Err(e) =>
						{
							if symphonia_decode_error_is_recoverable(&e)
							{
								continue;
							}
							return Err(format!("解码错误: {:?}", e));
						}
					}
				}
				Ok(None) =>
				{
					if let Some(ref mut dts) = self.dts_in_pcm
					{
						dts.set_input_eof();
					}
					continue;
				}
				Err(e) => return Err(format!("解码错误: {:?}", e)),
			}
		}
	}
}

impl audio_dec for SymphoniaDecoder {
	fn info(&self) -> DecoderInfo {
		DecoderInfo {
			sample_rate: self.sample_rate,
			channels: self.channels,
			bits_per_sample: self.bits_per_sample,
			duration_ms: self.duration_ms,
			codec_name: self.codec_name,
		}
	}

	fn decode_block(&mut self, _max_frames: usize) -> Result<Vec<f64>, String> {
		if self.dts_in_pcm.is_some()
		{
			return self.decode_block_dts_in_pcm();
		}

		loop
		{
			match self.format.next_packet()
			{
				Ok(Some(packet)) =>
				{
					if packet.track_id != self.track_id
					{
						continue;
					}

					match self.decoder.decode(&packet)
					{
						Ok(decoded) =>
						{
							if !self.pcm_probe_checked && decoded.spec().channels().count() == 2
							{
								self.pcm_probe_checked = true;

								let max_frames = decoded.frames().min(8192);
								let mut hit: Option<usize> = None;
								match decoded.clone()
								{
									GenericAudioBufferRef::S16(buf) =>
									{
										if let Some((ch0, ch1)) = buf.plane_pair(0, 1)
										{
											let mut i = 0usize;
											let mut right = false;
											let words = std::iter::from_fn(|| {
												if i >= max_frames
												{
													return None;
												}
												let w = if !right
												{
													right = true;
													ch0[i] as u16
												}
												else
												{
													right = false;
													let w = ch1[i] as u16;
													i = i.saturating_add(1);
													w
												};
												Some(w)
											});
											hit = pcm_probe_dts_in_s16le_words(words);
										}
									}
									GenericAudioBufferRef::S32(buf) =>
									{
										if let Some((ch0, ch1)) = buf.plane_pair(0, 1)
										{
											let mut i = 0usize;
											let mut right = false;
											let words = std::iter::from_fn(|| {
												if i >= max_frames
												{
													return None;
												}
												let w = if !right
												{
													right = true;
													((ch0[i] >> 16) as i16) as u16
												}
												else
												{
													right = false;
													let w = ((ch1[i] >> 16) as i16) as u16;
													i = i.saturating_add(1);
													w
												};
												Some(w)
											});
											hit = pcm_probe_dts_in_s16le_words(words);
										}
									}
									_ =>
									{}
								}

								if let Some(offset_words) = hit
								{
									eprintln!("[dec] PCM 探测: 检测到 DTS-in-PCM (offset_words={})，启用二次解码", offset_words);
									self.dts_in_pcm = Some(unsafe { FfmpegDtsInPcmDecoder::new()? });
									if let Some(ref mut dts) = self.dts_in_pcm
									{
										Self::feed_dts_in_pcm_from_decoded(dts, &decoded)?;
									}
									drop(decoded);
									return self.decode_block_dts_in_pcm();
								}
							}

							let mut samples: Vec<f64> = Vec::with_capacity(decoded.samples_interleaved());
							decoded.copy_to_vec_interleaved(&mut samples);
							return Ok(samples);
						}
						Err(e) =>
						{
							if symphonia_decode_error_is_recoverable(&e)
							{
								continue;
							}
							return Err(format!("解码错误: {:?}", e));
						}
					}
				}
				Ok(None) => return Ok(Vec::new()), // EOF
				Err(e) => return Err(format!("解码错误: {:?}", e)),
			}
		}
	}

	fn seek_ms(&mut self, ms: i64) -> Result<(), String> {
		let ms = ms.max(0) as u64;

		// ms -> TimeStamp(ticks) in track time_base units
		let ts_rel = {
			let ms = ms as u128;
			let denom = self.time_base.denom.get() as u128;
			let numer = self.time_base.numer.get() as u128;
			let ts = (ms.saturating_mul(denom)) / (1000u128.saturating_mul(numer));
			ts as u64
		};
		let ts = self
			.stream_first_ts
			.saturating_add(Duration::new(ts_rel));

		// 非 0 开始播放常见的“嚓嚓/咔嚓”瞬态，很多时候来自“粗略 seek”落在更早的 packet 边界，
		// 或者解码器需要 preroll 才能在目标点输出稳定样本。
		// 优先用 Accurate seek（成本只在 seek 时支付），失败再回退到 Coarse。
		if let Err(e_acc) = self
			.format
			.seek(SeekMode::Accurate, SeekTo::Timestamp { ts, track_id: self.track_id })
		{
			eprintln!("[dec] Seek Accurate 失败，回退 Coarse: {:?}", e_acc);
			self.format
				.seek(SeekMode::Coarse, SeekTo::Timestamp { ts, track_id: self.track_id })
				.map_err(|e| format!("Seek 失败: {:?}", e))?;
		}

		self.decoder.reset();

		if let Some(ref mut dts) = self.dts_in_pcm
		{
			unsafe { dts.reset() }?;
		}
		Ok(())
	}
}

// src\device_notify.rs
// 设备通知：监听默认输出设备变化，触发无缝切换

// 去抖：默认输出设备在蓝牙设备开关机时可能连续变更多次
const DEFAULT_DEVICE_DEBOUNCE_MS: u32 = 300;
const DEFAULT_DEVICE_DEBOUNCE_POLL_MS: u32 = 20;
const DEFAULT_DEVICE_DEBOUNCE_MAX_MS: u32 = 5000;

#[implement(IMMNotificationClient)]
struct EndpointNotificationClient;

impl EndpointNotificationClient {
	fn new() -> Self {
		Self
	}
}

#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for EndpointNotificationClient_Impl {
	fn OnDeviceStateChanged(&self, _pwstr_device_id: &PCWSTR, _dw_new_state: DEVICE_STATE) -> WinResult<()> {
		Ok(())
	}

	fn OnDeviceAdded(&self, _pwstr_device_id: &PCWSTR) -> WinResult<()> {
		Ok(())
	}

	fn OnDeviceRemoved(&self, _pwstr_device_id: &PCWSTR) -> WinResult<()> {
		Ok(())
	}

	fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _pwstr_default_device_id: &PCWSTR) -> WinResult<()> {
		unsafe {
			if flow == eRender && role == eConsole
			{
				let now = GetTickCount();
				g_device_change_tick.store(now, Ordering::SeqCst);

				if get_player_state() == PlayerState::Playing
				{
					// 只打印一次，避免事件风暴刷屏
					if request_playback_retry(RetryReason::DefaultDeviceChanged)
					{
						eprintln!("[device] 默认输出设备变更，准备切换");
					}
				}
			}
			Ok(())
		}
	}

	fn OnPropertyValueChanged(&self, _pwstr_device_id: &PCWSTR, _key: &PROPERTYKEY) -> WinResult<()> {
		Ok(())
	}
}

/// 等待默认输出设备“稳定”一小段时间（防止蓝牙开机时连续默认设备变更导致反复重建）
unsafe fn wait_for_default_render_device_settle() {
	let start = GetTickCount();
	loop
	{
		let last = g_device_change_tick.load(Ordering::SeqCst);
		if last == 0
		{
			break;
		}

		let now = GetTickCount();
		if now.wrapping_sub(last) >= DEFAULT_DEVICE_DEBOUNCE_MS
		{
			break;
		}
		if now.wrapping_sub(start) >= DEFAULT_DEVICE_DEBOUNCE_MAX_MS
		{
			eprintln!("[device] 等待默认输出设备稳定超时，继续重建");
			break;
		}

		Sleep(DEFAULT_DEVICE_DEBOUNCE_POLL_MS);
	}

	// 清理残留的切换请求，避免刚重建就立刻再次触发一次无意义重建
	clear_playback_retry_request_if(RetryReason::DefaultDeviceChanged);
}

// src\dir_li.rs
unsafe fn dir_to_list(pl: pl_em) {
	let pl_em::st_usize(path, li_id) = pl
	else
	{
		return;
	};

	let (files, pos) = collect_dir_playlist_from_path(&path);
	let len = files.len();
	if len == 0
	{
		return;
	}

	// 更新 UI 播放列表（在更新池之前克隆，避免锁冲突）
	let songs = collect_playlist_song_info(&files);
	let songs_for_ui = songs.clone();

	{
		let mut pool = m_pl_pool.write().unwrap();
		let Some(li) = pool.get_mut(&li_id)
		else
		{
			return;
		};
		*li = songs;
	}

	// 保存完整列表到数据库
	// auto_dir 只是更新列表，不创建新列表
	if !db_replace_playlist(li_id, None, &songs_for_ui)
	{
		eprintln!("[auto_dir] 保存播放列表失败: li_id={}", li_id);
		return;
	}
	// 只有当当前播放列表仍是此列表时才更新状态
	if g_li_id.load(Ordering::SeqCst) == li_id
	{
		g_track.store(pos, Ordering::SeqCst);
		let track_path_owned = songs_for_ui
			.get(pos)
			.map(|s| s.path.clone());

		let now_playing_meta = songs_for_ui.get(pos).map(|s| s.clone());

		// 更新 UI 播放列表显示并选中当前曲目
		ui_playlist_update(li_id, songs_for_ui);
		ui_playlist_select(li_id, pos);

		if let Some(song) = now_playing_meta
		{
			let track_path = track_path_owned
				.as_deref()
				.unwrap_or("");
			ui_set_now_playing2(li_id, pos, &song);
		}

		// 更新数据库中的播放状态
		let mode = g_pl_mode.load(Ordering::SeqCst);
		let volume = g_to_volume.load(Ordering::SeqCst);
		let track_path = track_path_owned.as_deref();
		db_save_state(li_id as i64, pos, track_path, 0, mode, volume);
	}

	if g_ev_li_chang != 0
	{
		SetEvent(g_ev_li_chang);
	}

	eprintln!("[auto_dir] 已加载目录播放列表: li_id={}, pos={}, total={}", li_id, pos + 1, len);
}

unsafe fn collect_dir_playlist_from_path(path: &str) -> (Vec<String>, usize) {
	let path_fixed = path.replace('/', "\\");
	let path = path_fixed.as_str();

	let Some((dir, _name)) = path.rsplit_once('\\')
	else
	{
		return (vec![path.to_string()], 0);
	};

	let pattern = format!("{}\\*", dir);
	let mut p: WIN32_FIND_DATAW = zeroed();
	let h = FindFirstFileW(to_wstring(&pattern).as_ptr(), &mut p);
	if h == -1
	{
		return (vec![path.to_string()], 0);
	}

	let mut files: Vec<String> = Vec::new();
	loop
	{
		if (p.dwFileAttributes & 16) == 0
		{
			let n = String::from_utf16_lossy(&p.cFileName[..get_dir_u16(&p.cFileName)]);
			if let Some((_, ext)) = n.rsplit_once('.')
			{
				let ext = ext.to_lowercase();
				let size_is_zero = p.nFileSizeHigh == 0 && p.nFileSizeLow == 0;
				if !size_is_zero && is_supported_media_ext(&ext)
				{
					let mut full = String::with_capacity(dir.len() + n.len() + 1);
					full.push_str(dir);
					full.push('\\');
					full.push_str(n.as_str());
					files.push(full);
				}
			}
		}

		if 0 == FindNextFileW(h, &mut p)
		{
			break;
		}
	}
	FindClose(h);

	if files.is_empty()
	{
		return (vec![path.to_string()], 0);
	}

	files.sort_unstable();

	let target = normalize_path_key(path);
	let pos = files
		.iter()
		.position(|p| normalize_path_key(p) == target)
		.unwrap_or(0);

	(files, pos)
}

// src\dir_to.rs
fn is_supported_media_ext(ext: &str) -> bool {
	match ext
	{
		"mp3" | "flac" | "wav" | "m4a" | "aac" | "ogg" | "wma" | "opus" | "ape" | "alac" | "tta" | "dsd" | "dsf" | "dff" | "aiff"
		| "aif" | "wv" | "mka" | "mp2" | "ac3" | "dts" => true,
		_ => false,
	}
}

unsafe fn get_dir_next(path: &str) -> String {
	let Some((dir, name)) = path.rsplit_once('\\')
	else
	{
		return path.to_string();
	};

	let pattern = format!("{}\\*", dir);
	let mut p: WIN32_FIND_DATAW = zeroed();
	let h = FindFirstFileW(to_wstring(&pattern).as_ptr(), &mut p);
	if h == -1
	{
		return path.to_string();
	}

	let mut found_current = false;
	let mut first: Option<String> = None;

	loop
	{
		if (p.dwFileAttributes & 16) == 0
		{
			let n = String::from_utf16_lossy(&p.cFileName[..get_dir_u16(&p.cFileName)]);
			if let Some((_, ext)) = n.rsplit_once('.')
			{
				let ext = ext.to_lowercase();
				let size_is_zero = p.nFileSizeHigh == 0 && p.nFileSizeLow == 0;
				if !size_is_zero && is_supported_media_ext(&ext)
				{
					let mut full = String::with_capacity(dir.len() + n.len() + 1);
					full.push_str(dir);
					full.push('\\');
					full.push_str(n.as_str());

					if first.is_none()
					{
						first = Some(full.clone());
					}

					if found_current
					{
						FindClose(h);
						return full;
					}
					if n == name
					{
						found_current = true;
					}
				}
			}
		}

		if 0 == FindNextFileW(h, &mut p)
		{
			break;
		}
	}

	FindClose(h);
	first.unwrap_or_else(|| path.to_string())
}

unsafe fn get_dir_prev(path: &str) -> String {
	let Some((dir, name)) = path.rsplit_once('\\')
	else
	{
		return path.to_string();
	};

	let pattern = format!("{}\\*", dir);
	let mut p: WIN32_FIND_DATAW = zeroed();
	let h = FindFirstFileW(to_wstring(&pattern).as_ptr(), &mut p);
	if h == -1
	{
		return path.to_string();
	}

	let mut prev: Option<String> = None;
	let mut last: Option<String> = None;
	let mut found_current = false;

	loop
	{
		if (p.dwFileAttributes & 16) == 0
		{
			let n = String::from_utf16_lossy(&p.cFileName[..get_dir_u16(&p.cFileName)]);
			if let Some((_, ext)) = n.rsplit_once('.')
			{
				let ext = ext.to_lowercase();
				let size_is_zero = p.nFileSizeHigh == 0 && p.nFileSizeLow == 0;
				if !size_is_zero && is_supported_media_ext(&ext)
				{
					let mut full = String::with_capacity(dir.len() + n.len() + 1);
					full.push_str(dir);
					full.push('\\');
					full.push_str(n.as_str());

					if n == name
					{
						found_current = true;
						break;
					}

					prev = Some(full.clone());
					last = Some(full);
				}
			}
		}

		if 0 == FindNextFileW(h, &mut p)
		{
			break;
		}
	}

	FindClose(h);

	if found_current
	{
		prev.or(last)
			.unwrap_or_else(|| path.to_string())
	}
	else
	{
		last.unwrap_or_else(|| path.to_string())
	}
}

fn get_dir_u16(m: &[u16; 260]) -> usize {
	for (i, &n) in m.iter().enumerate()
	{
		if n == 0
		{
			return i;
		}
	}
	260
}

#[repr(C)]
struct WIN32_FIND_DATAW {
	dwFileAttributes: u32,
	ftCreationTime: FILETIME,
	ftLastAccessTime: FILETIME,
	ftLastWriteTime: FILETIME,
	nFileSizeHigh: u32,
	nFileSizeLow: u32,
	dwReserved0: u32,
	dwReserved1: u32,
	cFileName: [u16; 260],
	cAlternateFileName: [u16; 14],
}

#[derive(Clone, Debug)]
#[repr(C)]
struct FILETIME {
	dwLowDateTime: u32,
	dwHighDateTime: u32,
}

// src\ffmpeg_init.rs
// FFmpeg 解码器模块
// 使用动态加载 FFmpeg DLL 支持 WMA 等格式
// 结构体定义严格按照 FFmpeg 8.0 (avcodec-62) 官方头文件

// === FFmpeg 结构体定义 (FFmpeg 8.0 / Master) ===
// 注意：无 修饰，匹配项目全局作用域规则

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct AVRational {
	num: i32,
	den: i32,
}

#[repr(C)]
struct AVCodecParameters {
	codec_type: i32,
	codec_id: i32,
	codec_tag: u32,
	extradata: *mut u8,
	extradata_size: i32,
	coded_side_data: *mut i64,
	nb_coded_side_data: i32,
	format: i32,
	bit_rate: i64,
	bits_per_coded_sample: i32,
	bits_per_raw_sample: i32,
	profile: i32,
	level: i32,
	width: i32,
	height: i32,
	sample_aspect_ratio: AVRational,
	framerate: AVRational,
	// ... 后续字段省略
}

#[repr(C)]
struct AVStream {
	av_class: *const i64,
	index: i32,
	id: i32,
	codecpar: *mut AVCodecParameters,
	priv_data: *mut i64,
	time_base: AVRational,
	start_time: i64,
	duration: i64,
	nb_frames: i64,
	disposition: i32,
	discard: i32,
	sample_aspect_ratio: AVRational,
	metadata: *mut i64,
	avg_frame_rate: AVRational,
	attached_pic: AVPacket,
	// ...
}

#[repr(C)]
struct AVFormatContext {
	av_class: *const i64,
	iformat: *const i64,
	oformat: *const i64,
	priv_data: *mut i64,
	pb: *mut i64,
	ctx_flags: i32,
	nb_streams: u32,
	streams: *mut *mut AVStream,
	nb_stream_groups: u32,
	stream_groups: *mut *mut i64, // AVStreamGroup**
	nb_chapters: u32,
	chapters: *mut *mut i64, // AVChapter**
	url: *mut i8,
	start_time: i64,
	duration: i64,
	bit_rate: i64,
	packet_size: u32,
	max_delay: i32,
	flags: i32,
	probesize: i64,
	max_analyze_duration: i64,
	key: *const u8,
	keylen: i32,
	nb_programs: u32,
	programs: *mut *mut i64, // AVProgram**
	video_codec_id: i32,     // enum AVCodecID
	audio_codec_id: i32,
	subtitle_codec_id: i32,
	data_codec_id: i32,
	metadata: *mut i64, // AVDictionary*
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AVChannelLayout {
	order: i32, // enum AVChannelOrder
	nb_channels: i32,
	u: AVChannelLayoutU,
	opaque: *mut i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
union AVChannelLayoutU {
	mask: u64,
	map: *mut i64,
}

#[repr(C)]
struct AVCodecContext {
	av_class: *const i64,                            // 0
	log_level_offset: i32,                           // 8
	codec_type: i32,                                 // 12
	codec: *const i64,                               // 16
	codec_id: i32,                                   // 24
	codec_tag: u32,                                  // 28
	priv_data: *mut i64,                             // 32
	internal: *mut i64,                              // 40
	opaque: *mut i64,                                // 48
	bit_rate: i64,                                   // 56
	flags: i32,                                      // 64
	flags2: i32,                                     // 68
	extradata: *mut u8,                              // 72
	extradata_size: i32,                             // 80
	time_base: AVRational,                           // 84
	pkt_timebase: AVRational,                        // 92
	framerate: AVRational,                           // 100
	delay: i32,                                      // 108
	width: i32,                                      // 112
	height: i32,                                     // 116
	coded_width: i32,                                // 120
	coded_height: i32,                               // 124
	sample_aspect_ratio: AVRational,                 // 128
	pix_fmt: i32,                                    // 136
	sw_pix_fmt: i32,                                 // 140
	color_primaries: i32,                            // 144
	color_trc: i32,                                  // 148
	colorspace: i32,                                 // 152
	color_range: i32,                                // 156
	chroma_sample_location: i32,                     // 160
	field_order: i32,                                // 164
	refs: i32,                                       // 168
	has_b_frames: i32,                               // 172
	slice_flags: i32,                                // 176
	draw_horiz_band: Option<unsafe extern "C" fn()>, // 184 (pointer)
	get_format: Option<unsafe extern "C" fn()>,      // 192 (pointer)
	max_b_frames: i32,                               // 200
	b_quant_factor: f32,                             // 204
	b_quant_offset: f32,                             // 208
	i_quant_factor: f32,                             // 212
	i_quant_offset: f32,                             // 216
	lumi_masking: f32,                               // 220
	temporal_cplx_masking: f32,                      // 224
	spatial_cplx_masking: f32,                       // 228
	p_masking: f32,                                  // 232
	dark_masking: f32,                               // 236
	nsse_weight: i32,                                // 240
	me_cmp: i32,                                     // 244
	me_sub_cmp: i32,                                 // 248
	mb_cmp: i32,                                     // 252
	ildct_cmp: i32,                                  // 256
	dia_size: i32,                                   // 260
	last_predictor_count: i32,                       // 264
	me_pre_cmp: i32,                                 // 268
	pre_dia_size: i32,                               // 272
	me_subpel_quality: i32,                          // 276
	me_range: i32,                                   // 280
	mb_decision: i32,                                // 284
	intra_matrix: *mut u16,                          // 288
	inter_matrix: *mut u16,                          // 296
	chroma_intra_matrix: *mut u16,                   // 304
	intra_dc_precision: i32,                         // 312
	mb_lmin: i32,                                    // 316
	mb_lmax: i32,                                    // 320
	bidir_refine: i32,                               // 324
	keyint_min: i32,                                 // 328
	gop_size: i32,                                   // 332
	mv0_threshold: i32,                              // 336
	slices: i32,                                     // 340
	// Audio only
	sample_rate: i32, // 344
	sample_fmt: i32,  // 348
	ch_layout: AVChannelLayout, // 352
	                  // ...
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AVPacket {
	buf: *mut i64,
	pts: i64,
	dts: i64,
	data: *mut u8,
	size: i32,
	stream_index: i32,
	flags: i32,
	side_data: *mut i64,
	side_data_elems: i32,
	duration: i64,
	pos: i64,
	opaque: *mut i64,
	opaque_ref: *mut i64,
	time_base: AVRational,
}

#[repr(C)]
struct AVFrame {
	data: [*mut u8; 8],
	linesize: [i32; 8],
	extended_data: *mut *mut u8,
	width: i32,
	height: i32,
	nb_samples: i32,
	format: i32,
	key_frame: i32,
	pict_type: i32,
	sample_aspect_ratio: AVRational,
	pts: i64,
	pkt_dts: i64,
	time_base: AVRational,
	// ...
}

// 保留未完全定义的结构体占位（只读取 name/long_name）
#[repr(C)]
struct AVCodec {
	name: *const i8,
	long_name: *const i8,
}
enum AVInputFormat {}
enum AVDictionary {}

#[repr(C)]
struct AVDictionaryEntry {
	key: *const i8,
	value: *const i8,
}

// === 函数指针类型 ===
type AvFormatOpenInput = unsafe extern "C" fn(*mut *mut AVFormatContext, *const i8, *const AVInputFormat, *mut *mut AVDictionary) -> i32;
type AvFormatFindStreamInfo = unsafe extern "C" fn(*mut AVFormatContext, *mut *mut AVDictionary) -> i32;
type AvReadFrame = unsafe extern "C" fn(*mut AVFormatContext, *mut AVPacket) -> i32;
type AvFormatCloseInput = unsafe extern "C" fn(*mut *mut AVFormatContext);
type AvFindBestStream = unsafe extern "C" fn(*mut AVFormatContext, i32, i32, i32, *mut *const AVCodec, i32) -> i32;
type AvSeekFrame = unsafe extern "C" fn(*mut AVFormatContext, i32, i64, i32) -> i32;
type AvDictGet = unsafe extern "C" fn(*const AVDictionary, *const i8, *const AVDictionaryEntry, i32) -> *mut AVDictionaryEntry;
type AvLogSetLevel = unsafe extern "C" fn(i32);

type AvCodecAllocContext3 = unsafe extern "C" fn(*const AVCodec) -> *mut AVCodecContext;
type AvCodecParametersToContext = unsafe extern "C" fn(*mut AVCodecContext, *const AVCodecParameters) -> i32;
type AvCodecOpen2 = unsafe extern "C" fn(*mut AVCodecContext, *const AVCodec, *mut *mut AVDictionary) -> i32;
type AvCodecFreeContext = unsafe extern "C" fn(*mut *mut AVCodecContext);
type AvCodecSendPacket = unsafe extern "C" fn(*mut AVCodecContext, *const AVPacket) -> i32;
type AvCodecReceiveFrame = unsafe extern "C" fn(*mut AVCodecContext, *mut AVFrame) -> i32;
type AvCodecFlushBuffers = unsafe extern "C" fn(*mut AVCodecContext);
type AvcodecFindDecoder = unsafe extern "C" fn(i32) -> *const AVCodec;
type AvcodecFindDecoderByName = unsafe extern "C" fn(*const i8) -> *const AVCodec;

enum AVCodecParserContext {}

type AvParserInit = unsafe extern "C" fn(i32) -> *mut AVCodecParserContext;
type AvParserParse2 =
	unsafe extern "C" fn(*mut AVCodecParserContext, *mut AVCodecContext, *mut *mut u8, *mut i32, *const u8, i32, i64, i64, i64) -> i32;
type AvParserClose = unsafe extern "C" fn(*mut AVCodecParserContext);

type AvPacketAlloc = unsafe extern "C" fn() -> *mut AVPacket;
type AvPacketUnref = unsafe extern "C" fn(*mut AVPacket);
type AvPacketFree = unsafe extern "C" fn(*mut *mut AVPacket);

type AvFrameAlloc = unsafe extern "C" fn() -> *mut AVFrame;
type AvFrameUnref = unsafe extern "C" fn(*mut AVFrame);
type AvFrameFree = unsafe extern "C" fn(*mut *mut AVFrame);

// === 函数集合 ===
struct AvFunctions {
	avformat_open_input: AvFormatOpenInput,
	avformat_find_stream_info: AvFormatFindStreamInfo,
	av_read_frame: AvReadFrame,
	avformat_close_input: AvFormatCloseInput,
	av_find_best_stream: AvFindBestStream,
	av_seek_frame: AvSeekFrame,
	av_dict_get: AvDictGet,
	av_log_set_level: AvLogSetLevel,

	avcodec_alloc_context3: AvCodecAllocContext3,
	avcodec_parameters_to_context: AvCodecParametersToContext,
	avcodec_open2: AvCodecOpen2,
	avcodec_free_context: AvCodecFreeContext,
	avcodec_send_packet: AvCodecSendPacket,
	avcodec_receive_frame: AvCodecReceiveFrame,
	avcodec_flush_buffers: AvCodecFlushBuffers,
	avcodec_find_decoder: AvcodecFindDecoder,
	avcodec_find_decoder_by_name: AvcodecFindDecoderByName,
	av_parser_init: AvParserInit,
	av_parser_parse2: AvParserParse2,
	av_parser_close: AvParserClose,

	av_packet_alloc: AvPacketAlloc,
	av_packet_unref: AvPacketUnref,
	av_packet_free: AvPacketFree,

	av_frame_alloc: AvFrameAlloc,
	av_frame_unref: AvFrameUnref,
	av_frame_free: AvFrameFree,
}

// DLL 路径

static mut g_ffm_dir: String = String::new();

unsafe fn ffmpeg_load_func(h: i64, name: &[u8]) -> i64 {
	// 使用 sys.rs 中的全局 GetProcAddress
	let p = GetProcAddress(h, name.as_ptr());
	if p == 0
	{
		panic!("FFmpeg: 加载函数失败: {:?}", from_utf8(name));
	}
	p
}

// 延迟加载 FFmpeg 函数
// LazyLock 假定在全局 (0.rs 中引入)
static AV_FUNCS: LazyLock<Option<AvFunctions>> = LazyLock::new(|| unsafe {
	if g_ffm_dir.is_empty()
	{
		g_ffm_dir.push_str(r"D:\float\OneDrive\diatom\conf\exe\ffmpeg");
	};

	eprintln!("[ffm] ffm_dir: {}", g_ffm_dir);

	let path_util = format!("{}\\avutil-60.dll", g_ffm_dir);
	let path_codec = format!("{}\\avcodec-62.dll", g_ffm_dir);
	let path_format = format!("{}\\avformat-62.dll", g_ffm_dir);

	// 使用 sys.rs 中的全局 LoadLibraryW
	let h_util = LoadLibraryW(to_wstring(&path_util).as_ptr());
	if h_util == 0
	{
		eprintln!("FFmpeg: 无法加载 {}", path_util);
		return None;
	}
	let h_codec = LoadLibraryW(to_wstring(&path_codec).as_ptr());
	if h_codec == 0
	{
		eprintln!("FFmpeg: 无法加载 {}", path_codec);
		return None;
	}
	let h_format = LoadLibraryW(to_wstring(&path_format).as_ptr());
	if h_format == 0
	{
		eprintln!("FFmpeg: 无法加载 {}", path_format);
		return None;
	}

	let funcs = AvFunctions {
		avformat_open_input: transmute(ffmpeg_load_func(h_format, b"avformat_open_input\0")),
		avformat_find_stream_info: transmute(ffmpeg_load_func(h_format, b"avformat_find_stream_info\0")),
		av_read_frame: transmute(ffmpeg_load_func(h_format, b"av_read_frame\0")),
		avformat_close_input: transmute(ffmpeg_load_func(h_format, b"avformat_close_input\0")),
		av_find_best_stream: transmute(ffmpeg_load_func(h_format, b"av_find_best_stream\0")),
		av_seek_frame: transmute(ffmpeg_load_func(h_format, b"av_seek_frame\0")),
		av_dict_get: transmute(ffmpeg_load_func(h_util, b"av_dict_get\0")),
		av_log_set_level: transmute(ffmpeg_load_func(h_util, b"av_log_set_level\0")),

		avcodec_alloc_context3: transmute(ffmpeg_load_func(h_codec, b"avcodec_alloc_context3\0")),
		avcodec_parameters_to_context: transmute(ffmpeg_load_func(h_codec, b"avcodec_parameters_to_context\0")),
		avcodec_open2: transmute(ffmpeg_load_func(h_codec, b"avcodec_open2\0")),
		avcodec_free_context: transmute(ffmpeg_load_func(h_codec, b"avcodec_free_context\0")),
		avcodec_send_packet: transmute(ffmpeg_load_func(h_codec, b"avcodec_send_packet\0")),
		avcodec_receive_frame: transmute(ffmpeg_load_func(h_codec, b"avcodec_receive_frame\0")),
		avcodec_flush_buffers: transmute(ffmpeg_load_func(h_codec, b"avcodec_flush_buffers\0")),
		avcodec_find_decoder: transmute(ffmpeg_load_func(h_codec, b"avcodec_find_decoder\0")),
		avcodec_find_decoder_by_name: transmute(ffmpeg_load_func(h_codec, b"avcodec_find_decoder_by_name\0")),
		av_parser_init: transmute(ffmpeg_load_func(h_codec, b"av_parser_init\0")),
		av_parser_parse2: transmute(ffmpeg_load_func(h_codec, b"av_parser_parse2\0")),
		av_parser_close: transmute(ffmpeg_load_func(h_codec, b"av_parser_close\0")),

		av_packet_alloc: transmute(ffmpeg_load_func(h_codec, b"av_packet_alloc\0")),
		av_packet_unref: transmute(ffmpeg_load_func(h_codec, b"av_packet_unref\0")),
		av_packet_free: transmute(ffmpeg_load_func(h_codec, b"av_packet_free\0")),

		av_frame_alloc: transmute(ffmpeg_load_func(h_util, b"av_frame_alloc\0")),
		av_frame_unref: transmute(ffmpeg_load_func(h_util, b"av_frame_unref\0")),
		av_frame_free: transmute(ffmpeg_load_func(h_util, b"av_frame_free\0")),
	};

	(funcs.av_log_set_level)(AV_LOG_ERROR);

	Some(funcs)
});

const AVMEDIA_TYPE_AUDIO: i32 = 1;
const AV_DISPOSITION_ATTACHED_PIC: i32 = 0x0400;
const AV_DICT_IGNORE_SUFFIX: i32 = 2;
const AV_LOG_ERROR: i32 = 16;

// src\init.rs
unsafe fn init() {
	let kill_hwnd = FindWindowW(MAIN_WIN_CLASS.as_ptr(), 0 as _);

	if kill_hwnd != 0
	{
		PostMessageW(kill_hwnd, WM_DESTROY, 0, 0);
	};

	wasapi::initialize_mta().unwrap();

	// 创建 RingBuffer 空间通知事件（自动重置）
	g_ev_ring_space = CreateEventW(0, 0, 0, 0 as _);
	// 创建恢复播放事件（手动重置，初始有信号=非暂停状态）
	g_ev_resume = CreateEventW(0, 1, 1, 0 as _);
	// 创建解码线程唤醒事件（自动重置，初始无信号）
	g_ev_dec_wakeup = CreateEventW(0, 0, 0, 0 as _);
	// 创建解码线程空闲事件（手动重置，初始有信号=空闲）
	g_ev_dec_idle = CreateEventW(0, 1, 1, 0 as _);
	// 创建播放线程命令通知事件（自动重置，初始无信号）
	g_ev_pl_quit = CreateEventW(0, 0, 0, 0 as _);
	// 创建全局程序退出信号（手动重置，初始无信号）
	g_ev_app_quit = CreateEventW(0, 1, 0, 0 as _);
	// 创建播放列表变更事件（自动重置，初始无信号）
	g_ev_li_chang = CreateEventW(0, 0, 0, 0 as _);

	// https://www.monkeysaudio.com/developers.html
	g_mac_hd = LoadLibraryW(to_wstring(r"D:\float\OneDrive\diatom\conf\dll\audio\MACDll64.dll").as_ptr());

	mac_create_w = transmute(GetProcAddress(g_mac_hd, "c_APEDecompress_CreateW\0".as_ptr()));
	mac_destroy = transmute(GetProcAddress(g_mac_hd, "c_APEDecompress_Destroy\0".as_ptr()));
	mac_get_data = transmute(GetProcAddress(g_mac_hd, "c_APEDecompress_GetData\0".as_ptr()));
	mac_seek = transmute(GetProcAddress(g_mac_hd, "c_APEDecompress_Seek\0".as_ptr()));
	mac_get_info = transmute(GetProcAddress(g_mac_hd, "c_APEDecompress_GetInfo\0".as_ptr()));

	new_class(MAIN_WIN_CLASS.as_ptr(), window_proc);

	let hwnd = CreateWindowExW(0x00000080, MAIN_WIN_CLASS.as_ptr(), MAIN_WIN_CLASS.as_ptr(), 0x80000000, 0, 0, 1, 1, 0, 0, 0, 0);

	G_HWND = hwnd;

	if g_is_load_tray
	{
		g_wm_taskbar_created = RegisterWindowMessageW(to_wstring("TaskbarCreated").as_ptr());
		tray_add(hwnd);
	};

	ui_gdiplus_init();
	// 注册到系统媒体控制（SMTC），让蓝牙 AVRCP(耳机摘戴/按键) 能直接控制本进程播放状态
	init_smtc(hwnd);

	// 注册全局热键 (Ctrl+Alt+...)
	let ctrl_alt = 0x0002 | 0x0001; // MOD_CONTROL | MOD_ALT
	RegisterHotKey(hwnd, 1, ctrl_alt, 'P' as u32); // 暂停/播放
	RegisterHotKey(hwnd, 2, ctrl_alt, 'R' as u32); // 重新开始
	RegisterHotKey(hwnd, 3, ctrl_alt, 0x27); // 右箭头 - 快进
	RegisterHotKey(hwnd, 4, ctrl_alt, 0x25); // 左箭头 - 快退
	RegisterHotKey(hwnd, 5, ctrl_alt, 0x28); // 下箭头 - 下一首
	RegisterHotKey(hwnd, 6, ctrl_alt, 0x26); // 上箭头 - 上一首
	RegisterHotKey(hwnd, 7, ctrl_alt, 0xBB); // = 键 - 音量+
	RegisterHotKey(hwnd, 8, ctrl_alt, 0xBD); // - 键 - 音量-
	RegisterHotKey(hwnd, 9, ctrl_alt, 'E' as u32); // E 键 - 切换独占/共享模式

	// 监听蓝牙耳机/键盘的媒体键（例如 VK_MEDIA_PLAY_PAUSE）：用于实现摘戴自动暂停/恢复
	// install_media_key_hook();

	// 初始化数据库
	db_init();

	// 从 fog.db 读取播放列表到内存池（包括“用户”(1) / “默认”(2)）
	init_playlists_from_fog_db();
	taskbar_init();
}

unsafe fn ui_gdiplus_init() {
	let input =
		GdiplusStartupInput { gdiplus_version: 1, debug_event_callback: 0, suppress_background_thread: 0, suppress_external_codecs: 0 };
	let mut token: usize = 0;
	let st = GdiplusStartup(&mut token, &input, null_mut());
	if st != 0 || token == 0
	{
		eprintln!("[cover] GdiplusStartup failed: status={}", st);
	}
}

// src\log.rs
const UI_LOG_QUEUE_MAX: usize = 4096;
const UI_LOG_MAX_LINES: i64 = 1000;
const UI_LOG_TRIM_LINES: usize = 500;
const UI_LOG_EDIT_LIMIT_CHARS: usize = 0x7FFF_FFFE;

static UI_LOG_QUEUE: LazyLock<Mutex<VecDeque<String>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));
static UI_LOG_FLUSH_POSTED: AtomicBool = AtomicBool::new(false);

fn log_enqueue(text: String) {
	{
		let mut q = UI_LOG_QUEUE.lock().unwrap();
		while q.len() >= UI_LOG_QUEUE_MAX
		{
			q.pop_front();
		}
		q.push_back(text);
	}
	unsafe { ui_log_request_flush() };
}

unsafe fn ui_log_request_flush() {
	if !UI_LOG_FLUSH_POSTED.swap(true, Ordering::SeqCst)
	{
		PostMessageW(UI_HWND, WM_UI_LOG_FLUSH, 0, 0);
	}
}

unsafe fn ui_log_flush() {
	let pending: VecDeque<String> = {
		let mut q = UI_LOG_QUEUE.lock().unwrap();
		if q.is_empty()
		{
			UI_LOG_FLUSH_POSTED.store(false, Ordering::SeqCst);
			return;
		}
		take(&mut *q)
	};

	ui_log_append(&pending);

	UI_LOG_FLUSH_POSTED.store(false, Ordering::SeqCst);

	// If logs arrived during the flush, schedule another pass.
	if !UI_LOG_QUEUE.lock().unwrap().is_empty()
	{
		ui_log_request_flush();
	}
}

unsafe fn ui_log_append(lines: &VecDeque<String>) {
	if lines.is_empty()
	{
		return;
	}

	let mut sel_start: u32 = 0;
	let mut sel_end: u32 = 0;
	SendMessageW(UI_HLOG, EM_GETSEL, &mut sel_start as *mut _ as usize, &mut sel_end as *mut _ as i64);

	let len_before = SendMessageW(UI_HLOG, WM_GETTEXTLENGTH, 0, 0) as u32;
	let caret_at_end = sel_start == sel_end && sel_end == len_before;

	// Force append at end.
	SendMessageW(UI_HLOG, EM_SETSEL, len_before as usize, len_before as i64);

	let mut combined = String::new();
	combined.reserve(lines.iter().map(|s| s.len() + 2).sum());
	for line in lines
	{
		combined.push_str(line);
		combined.push('\r');
		combined.push('\n');
	}

	let mut ws: Vec<u16> = combined.encode_utf16().collect();
	ws.push(0);
	SendMessageW(UI_HLOG, EM_REPLACESEL, 0, ws.as_ptr() as i64);

	// 行数检测：超过阈值时删除前 500 行（循环处理突发批量追加，避免一次追加后仍超限）
	let mut removed_total: i64 = 0;
	loop
	{
		let line_count = SendMessageW(UI_HLOG, EM_GETLINECOUNT, 0, 0);
		if line_count <= UI_LOG_MAX_LINES
		{
			break;
		}

		let char_pos = SendMessageW(UI_HLOG, EM_LINEINDEX, UI_LOG_TRIM_LINES, 0);
		if char_pos <= 0
		{
			break;
		}

		SendMessageW(UI_HLOG, EM_SETSEL, 0, char_pos);
		let empty: [u16; 1] = [0];
		SendMessageW(UI_HLOG, EM_REPLACESEL, 0, empty.as_ptr() as i64);
		removed_total = removed_total.saturating_add(char_pos);
	}

	if caret_at_end
	{
		SendMessageW(UI_HLOG, EM_SCROLLCARET, 0, 0);
	}
	else
	{
		let mut new_start = sel_start as i64 - removed_total;
		let mut new_end = sel_end as i64 - removed_total;
		if new_start < 0
		{
			new_start = 0;
		}
		if new_end < 0
		{
			new_end = 0;
		}
		SendMessageW(UI_HLOG, EM_SETSEL, new_start as usize, new_end);
	}
}

// src\main.rs
static mut g_def_exclusive: bool = false;

fn main() {
	unsafe {
		conf_init();
		init();

		// 创建并显示 UI 窗口（播放列表和日志）
		ui_create_window();

		// 恢复播放状态（需要在 UI 窗口创建后调用，以便正确恢复窗口可见性）
		db_restore_playback();

		{
			let ring = HeapRb::<f64>::new(RING_BUFFER_CAPACITY);
			let (producer, consumer) = ring.split();
			let (decode_tx, decode_rx) = mpsc::channel::<DecodeCommand>();

			thread::spawn(move || {
				decode_thread(producer, decode_rx);
			});

			thread::spawn(move || {
				{
					// 在线程启动时注册 MMCSS（一次，对整个线程生效）
					let mut task_index = 0u32;

					if 0 == AvSetMmThreadCharacteristicsW(to_wstring("Pro Audio").as_ptr(), &mut task_index)
					{
						eprintln!("警告: 启用 MMCSS 失败");
					};
				}
				player_thread(decode_tx, consumer);
			});
		}

		for _ in 0..2
		{
			thread::spawn(|| {
				serve_receive();
			});
		}

		for _ in 0..3
		{
			thread::spawn(|| {
				pipe_music_call();
			});
		}

		{
			if !MUSIC_DB_PATH.is_empty() && !g_root_dir.is_empty()
			{
				thread::spawn(|| {
					music_scan_watch_thread();
				});
			}
		}

		// 注册 IMMNotificationClient：后台监听默认输出设备变化（蓝牙耳机断连/重连等）
		// 必须保证回调对象生命周期，否则系统回调可能触发 UAF 直接崩溃（无 Rust backtrace）。
		let mut t_imm = None;

		{
			match CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
			{
				Ok(enumerator) =>
				{
					let client: IMMNotificationClient = EndpointNotificationClient::new().into();

					if let Err(e) = enumerator.RegisterEndpointNotificationCallback(&client)
					{
						eprintln!("[device] 注册 IMMNotificationClient 失败: {:?}", e);
						return;
					}

					t_imm = Some((enumerator, client));

					eprintln!("[device] IMMNotificationClient 已注册");
				}
				Err(e) => eprintln!("[device] 创建设备枚举器失败: {:?}", e),
			}
		}

		let mut msg: MSG = core::mem::zeroed();

		while GetMessageW(&mut msg, 0, 0, 0) > 0
		{
			TranslateMessage(&msg);
			DispatchMessageW(&msg);
		}

		// 退出消息循环后取消注册
		if let Some((enumerator, client)) = t_imm
		{
			enumerator.UnregisterEndpointNotificationCallback(&client);
		};

		Sleep(20);
	}
}

// src\msg.rs
unsafe extern "system" fn window_proc(hwnd: i64, msg: u32, wparam: usize, lparam: i64) -> i64 {
	match msg
	{
		WM_PLAY_PAUSE =>
		{
			ui_wm_play_pause();
			0
		}
		WM_PAUSE =>
		{
			if get_player_state() != PlayerState::Playing
			{
				return 0;
			}

			let is_paused = WaitForSingleObject(g_ev_resume, 0) == 258;
			if !is_paused
			{
				ResetEvent(g_ev_resume);

				if g_is_exclusive.load(Ordering::SeqCst)
				{
					PostMessageW(0xFFFF, 51000, 1, G_HWND);
				}
			}
			0
		}
		WM_RESUME =>
		{
			if get_player_state() != PlayerState::Paused
			{
				return 0;
			}

			let is_paused = WaitForSingleObject(g_ev_resume, 0) == 258;
			if is_paused
			{
				SetEvent(g_ev_resume);

				if g_is_exclusive.load(Ordering::SeqCst)
				{
					PostMessageW(0xFFFF, 51000, 0, G_HWND);
				}
			}
			0
		}
		WM_RESTART =>
		{
			request_playback_retry(RetryReason::Restart);
			0
		}
		WM_SEEK_FWD =>
		{
			let delta = if lparam == 0 { 5 } else { (lparam as i32).min(100) };
			g_to_seek.fetch_add(delta, Ordering::SeqCst);
			0
		}
		WM_SEEK_BWD =>
		{
			let delta = if lparam == 0 { 5 } else { (lparam as i32).min(100) };
			g_to_seek.fetch_sub(delta, Ordering::SeqCst);
			0
		}
		WM_NEXT_TRACK =>
		{
			g_to_next.store(true, Ordering::SeqCst);

			// "下一首"在暂停时：直接取消暂停并切歌（用户预期）
			resume_if_paused();
			0
		}
		WM_PREV_TRACK =>
		{
			g_to_prev.store(true, Ordering::SeqCst);

			resume_if_paused();
			0
		}
		WM_RANDOM_NEXT_TRACK =>
		{
			let id = g_li_id.load(Ordering::SeqCst);
			let len = m_pl_pool
				.read()
				.unwrap()
				.get(&id)
				.map(|v| v.len())
				.unwrap_or(0);

			if len <= 1
			{
				return 0;
			}

			let cur = g_track
				.load(Ordering::SeqCst)
				.min(len - 1);
			let pick = random_next_track_index(id, cur, len);

			push_pl_cmd(PlayerCommand::SwitchToIndex(pick));
			resume_if_paused();
			0
		}
		WM_VOL_UP =>
		{
			let v = g_to_volume.load(Ordering::SeqCst);
			let nv = (v + 5).min(100);
			if nv != v
			{
				g_to_volume.store(nv, Ordering::SeqCst);
				ui_volume_sync(nv);
			}
			0
		}
		WM_VOL_DOWN =>
		{
			let v = g_to_volume.load(Ordering::SeqCst);
			let nv = v.saturating_sub(5);
			if nv != v
			{
				g_to_volume.store(nv, Ordering::SeqCst);
				ui_volume_sync(nv);
			}
			0
		}
		WM_TOGGLE_EXCLUSIVE =>
		{
			let was_exclusive = {
				let old = g_def_exclusive;
				g_def_exclusive = !old;
				old
			};

			request_playback_retry(RetryReason::ModeChanged);
			g_is_exclusive.store(!was_exclusive, Ordering::SeqCst); // 立即更新实际标志，避免误广播
			PostMessageW(0xFFFF, 51000, if was_exclusive { 1 } else { 0 }, G_HWND);

			eprintln!("[msg] 切换到 {} 模式", if was_exclusive { "共享" } else { "独占" });

			0
		}
		WM_DEVICE_IN_USE =>
		{
			eprintln!("[msg] 无法独占，暂停中");
			0
		}
		WM_PROGRESS =>
		{
			// wparam=当前位置(l), lparam=总时长(w)
			let l = wparam as u64; // 当前位置
			let w = if lparam > 0 { lparam as u64 } else { 0 }; // 总时长

			// 更新 UI 进度条

			if w > 0
			{
				let pos = ((l * 1000) / w).min(1000) as i32;
				SendMessageW(UI_HPROGRESS, PBM_SETPOS, pos as usize, 0);
				InvalidateRect(UI_HPROGRESS, null(), 1);
			}
			else
			{
				SendMessageW(UI_HPROGRESS, PBM_SETPOS, 0, 0);
				InvalidateRect(UI_HPROGRESS, null(), 1);
			}

			if g_ui_is_visible
			{
				if l == 0
				{
					taskbar_set_pos(g_TBL_PTR, UI_HWND, 1, 1000);
				}
				else
				{
					taskbar_set_pos(g_TBL_PTR, UI_HWND, l, w);
				};
			};

			// 定期保存播放进度到数据库 (每 5 秒)
			static mut LAST_SAVE_TIME: u64 = 0;
			let now = GetTickCount64();
			if now - LAST_SAVE_TIME > 5000
			{
				//smtc_sync_now_playing_if_needed();
				//smtc_update_timeline(l, w);

				let playlist_id = NOW_PLAYING_LI_ID.load(Ordering::SeqCst);
				let track_path = NOW_PLAYING
					.read()
					.ok()
					.map(|np| np.path.clone())
					.unwrap_or_default();

				if playlist_id > 0 && !track_path.is_empty()
				{
					// Use sample counters for persistence to avoid stale WM_PROGRESS messages
					// saving progress for a different (already switched) playlist/track.
					let progress_ms = {
						let samples = SAMPLES_PLAYED.load(Ordering::Relaxed);
						let sample_rate = OUTPUT_SAMPLE_RATE.load(Ordering::Relaxed);
						let channels = OUTPUT_CHANNELS.load(Ordering::Relaxed);
						if sample_rate > 0 && channels > 0 { (samples * 1000) / (sample_rate * channels) as u64 } else { l }
					};

					let key = normalize_path_key(&track_path);
					let track_idx = if let Ok(pool) = m_pl_pool.read()
						&& let Some(li) = pool.get(&playlist_id)
					{
						li.iter()
							.position(|s| normalize_path_key(&s.path) == key)
							.unwrap_or(0)
					}
					else
					{
						0
					};

					db_update_progress(playlist_id, track_idx, Some(track_path.as_str()), progress_ms);
					LAST_SAVE_TIME = now;
				}
			}

			0
		}
		WM_SMTC_STATUS =>
		{
			let state = match wparam as u8
			{
				1 => PlayerState::Playing,
				2 => PlayerState::Paused,
				3 => PlayerState::Stopped,
				4 => PlayerState::Error,
				_ => PlayerState::Idle,
			};

			smtc_update_playback_status(state);
			ui_play_state_sync(state == PlayerState::Playing);
			// 更新任务栏进度状态（播放/暂停/休止/空闲）

			if g_ui_is_visible
			{
				taskbar_sync_player_state(state);
			};

			0
		}
		WM_APPCOMMAND =>
		{
			let cmd = ((lparam >> 16) & 0xFFFF) as u32; // GET_APPCOMMAND_LPARAM
			match cmd
			{
				APPCOMMAND_MEDIA_PLAY_PAUSE => PostMessageW(hwnd, WM_PLAY_PAUSE, 0, 0),
				APPCOMMAND_MEDIA_PAUSE => PostMessageW(hwnd, WM_PAUSE, 0, 0),
				APPCOMMAND_MEDIA_PLAY => PostMessageW(hwnd, WM_RESUME, 0, 0),
				APPCOMMAND_MEDIA_NEXTTRACK => PostMessageW(hwnd, WM_NEXT_TRACK, 0, 0),
				APPCOMMAND_MEDIA_PREVIOUSTRACK => PostMessageW(hwnd, WM_PREV_TRACK, 0, 0),
				_ => return DefWindowProcW(hwnd, msg, wparam, lparam),
			};
			1
		}
		0x0312 =>
		{
			// WM_HOTKEY
			match wparam as u32
			{
				1 => PostMessageW(hwnd, WM_PLAY_PAUSE, 0, 0),
				2 => PostMessageW(hwnd, WM_RESTART, 0, 0),
				3 => PostMessageW(hwnd, WM_SEEK_FWD, 0, 0),
				4 => PostMessageW(hwnd, WM_SEEK_BWD, 0, 0),
				5 => PostMessageW(hwnd, WM_NEXT_TRACK, 0, 0),
				6 => PostMessageW(hwnd, WM_PREV_TRACK, 0, 0),
				7 => PostMessageW(hwnd, WM_VOL_UP, 0, 0),
				8 => PostMessageW(hwnd, WM_VOL_DOWN, 0, 0),
				9 => PostMessageW(hwnd, WM_TOGGLE_EXCLUSIVE, 0, 0),
				_ => 0,
			};
			0
		}
		WM_TRAYICON =>
		{
			tray_on_message(hwnd, wparam, lparam);
			0
		}
		WM_TOGGLE_WINDOW =>
		{
			ui_toggle_visibility();
			0
		}
		WM_DESTROY =>
		{
			// 退出前保存进度，避免短时间内退出/被新进程去重关闭导致进度回退
			save_current_progress();

			// 退出前保存状态
			let state = get_player_state();

			if state == PlayerState::Playing
			{
				db_save_playing_status(true);
			}
			else
			{
				db_save_playing_status(false);
			};

			tray_remove(hwnd);
			SetEvent(g_ev_app_quit);
			PostQuitMessage(0);
			0
		}

		_ =>
		{
			if msg == g_wm_taskbar_created && g_is_load_tray
			{
				tray_recreate(hwnd);
				0
			}
			else
			{
				DefWindowProcW(hwnd, msg, wparam, lparam)
			}
		}
	}
}

// src\music_info.rs
const PIPE_MUSIC_INFO: [u16; 22] = [92, 92, 46, 92, 112, 105, 112, 101, 92, 58, 92, 109, 117, 115, 105, 99, 95, 99, 97, 108, 108, 0]; //r"\\.\pipe\:\music_call"

#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct pipe_info_xy {
	x: i32,
	y: i32,
	flag: u16,
	hwnd: i64,
}

unsafe fn pipe_music_call() {
	loop
	{
		let pipe = CreateNamedPipeW(PIPE_MUSIC_INFO.as_ptr(), 3, 0, 255, 0, 0, 0, 0);
		ConnectNamedPipe(pipe, 0);

		let mut len = 0u32;
		let mut rv = [0u8; 24];

		if 0 == ReadFile(pipe, rv.as_mut_ptr(), 24, &mut len, null_mut())
		{
			CloseHandle(pipe);
			return;
		};

		if len == 24
		{
			let info = std::ptr::read_unaligned(rv.as_ptr() as *const pipe_info_xy);

			// 获取当前活动的播放列表 ID 作为默认值
			let pl_id = g_li_id.load(Ordering::SeqCst);
			let mut li = vec![("ms_ui_pl_id".to_string(), pl_id.to_string())];

			/*
			"syslistview32" => 1,
			"systreeview32" => 2,
			"systabcontrol32" => 3,
			"sysheader32" => 4,
			"toolbarwindow32" => 5,
			"rebarwindow32" => 6,
			"msctls_statusbar32" => 7,
			"msctls_progress32" => 8,
			"msctls_trackbar32" => 9,
			"msctls_updown32" => 10,
			"tooltips_class32" => 11,
			"msctls_hotkey32" => 12,
			"sysipaddress32" => 13,
			"sysdatetimepick32" => 14,
			"sysmonthcal32" => 15,
			"sysanimate32" => 16,
			"syslink" => 17,
			"richedit20a" => 18,
			"richedit20w" => 19,
			"richedit50w" => 20,
			*/
			if info.flag == 0
			{
				// 鼠标下没有控件，检测是否在标题栏
				// const WM_NCHITTEST: u32 = 0x0084;

				let lparam = ((info.y & 0xFFFF) << 16) | (info.x & 0xFFFF);
				let hit = SendMessageW(UI_HWND, 0x0084, 0, lparam as _);

				if hit == 2
				// HTCAPTION
				{
					li.push(("ms_ui_caption".to_string(), "1".to_owned()));
				}
			}
			else
			{
				let mut rect: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
				GetWindowRect(info.hwnd, &mut rect);

				let x = info.x - rect.left;
				let y = info.y - rect.top;

				match info.flag
				{
					1 =>
					{
						pipe_get_list(&mut li, info.hwnd, x, y);
					}
					2 =>
					{
						pipe_get_tree(&mut li, x, y);
						li.push(("ms_ui_tree".to_string(), "1".to_owned()));
					}
					3 =>
					{
						pipe_get_tab(&mut li, x, y);
						li.push(("ms_ui_tab".to_string(), "1".to_owned()));
					}
					4 =>
					{
						pipe_get_header(&mut li, info.hwnd, x, y);
					}
					_ =>
					{}
				};
			};

			let sv = vec2bin(&li);
			let sz = sv.len() as u32;

			WriteFile(pipe, &sz as *const _ as _, 4, null_mut(), null_mut());

			if sz > 0
			{
				WriteFile(pipe, sv.as_ptr(), sz, null_mut(), null_mut());
			}
		}
		else if len == 4
		{
			let sz = *(rv.as_ptr() as *const u32) as usize;
			if sz > 0
			{
				let mut rv = Vec::with_capacity(sz);
				rv.set_len(sz);

				if 0 == ReadFile(pipe, rv.as_mut_ptr(), sz as u32, &mut len, null_mut())
				{
					CloseHandle(pipe);
					return;
				};

				let cmd = bin2vec(&rv);
				pool_fn(pipe_cmd_handler, pl_em::cmd(cmd));

				WriteFile(pipe, 0u32.to_le_bytes().as_ptr(), 4, null_mut(), null_mut());
			};
		};

		CloseHandle(pipe);
	}
}

unsafe fn pipe_get_tab(li: &mut Vec<(String, String)>, x: i32, y: i32) {
	let Some(tab_idx) = ui_tab_xy_to_id(x, y)
	else
	{
		return;
	};

	if tab_idx >= ui_pl_views_li.len()
	{
		return;
	}

	// 标签名
	let name = ui_pl_views_li[tab_idx].name.clone();

	// 播放列表ID
	let pl_id = ui_pl_views_li[tab_idx].playlist_id;

	li.push(("ms_ui_tab_name".to_string(), name));
	li.push(("ms_ui_tab_pl_id".to_string(), pl_id.to_string()));
	li.push(("ms_ui_tab_id".to_string(), tab_idx.to_string()));
}

unsafe fn pipe_get_header(li: &mut Vec<(String, String)>, hwnd: i64, x: i32, y: i32) {
	// 查找匹配的播放列表视图

	let view = ui_pl_views_li
		.iter()
		.find(|v| v.hheader == hwnd);

	let Some(view) = view
	else
	{
		return;
	};

	let pl_id = view.playlist_id;
	li.push(("ms_ui_list_pl_id".to_string(), pl_id.to_string()));

	let mut hti: HDHITTESTINFO = HDHITTESTINFO { pt: POINT { x, y }, flags: 0, iItem: 0 };
	let hit = SendMessageW(hwnd, HDM_HITTEST, 0, &mut hti as *mut _ as i64);
	let col = hti.iItem;

	if hit >= 0 && col >= 0 && (hti.flags & HHT_ONHEADER) != 0 && (hti.flags & (HHT_ONDIVIDER | HHT_ONDIVOPEN)) == 0
	{
		let col_name = match col
		{
			0 => "#",
			1 => "歌曲名",
			2 => "时长",
			3 => "作者",
			4 => "专辑",
			_ if col == UI_PLAYLIST_INFO_COL_SUBITEM => "*",
			_ => "",
		};

		li.push(("ms_ui_list_col_id".to_string(), col.to_string()));
		li.push(("ms_ui_list_col_name".to_string(), col_name.to_string()));
		li.push(("ms_ui_head".to_string(), "1".to_owned()));
		return;
	}
}

unsafe fn pipe_get_list(li: &mut Vec<(String, String)>, hwnd: i64, x: i32, y: i32) {
	// 查找匹配的播放列表视图
	let view = ui_pl_views_li
		.iter()
		.find(|v| v.hlist == hwnd);

	let Some(view) = view
	else
	{
		return;
	};

	let pl_id = view.playlist_id;
	li.push(("ms_ui_list_pl_id".to_string(), pl_id.to_string()));

	// Hit test 获取项索引
	let mut ht: LVHITTESTINFO = LVHITTESTINFO { pt: POINT { x, y }, flags: 0, iItem: 0, iSubItem: 0, iGroup: 0 };

	let item_idx = SendMessageW(hwnd, LVM_HITTEST, 0, &mut ht as *mut _ as i64);

	if item_idx < 0
	{
		// 没有命中项，只返回列表ID
		return;
	}

	// 获取文件路径
	let path = if let Ok(pool) = m_pl_pool.read()
		&& let Some(songs) = pool.get(&pl_id)
		&& let Some(song) = songs.get(item_idx as usize)
	{
		song.path.clone()
	}
	else
	{
		String::new()
	};

	li.push(("ms_ui_list_item_idx".to_string(), item_idx.to_string()));
	li.push(("ms_ui_list_path".to_string(), path));
	li.push(("ms_ui_list".to_string(), "1".to_owned()));
}

unsafe fn pipe_get_tree(li: &mut Vec<(String, String)>, x: i32, y: i32) {
	let mut ht: TVHITTESTINFO = TVHITTESTINFO { pt: POINT { x, y }, flags: 0, hItem: 0 };

	SendMessageW(UI_HTREE, TVM_HITTEST, 0, &mut ht as *mut _ as i64);

	let hitem = ht.hItem;
	if hitem == 0 || (ht.flags & TVHT_ONITEM) == 0
	{
		return;
	}

	// 获取路径各部分
	let parts = ui_tree_item_path_parts(hitem);
	if parts.is_empty()
	{
		return;
	}

	// 项名 (最后一个部分)
	let name = parts
		.last()
		.cloned()
		.unwrap_or_default();

	li.push(("ms_ui_tree_name".to_string(), name));
	// 项类型: 有子项为父项, 否则为普通项
	li.push(("ms_ui_tree_type".to_string(), if ui_tree_item_child(hitem) != 0 { "parent".to_owned() } else { "normal".to_owned() }));
	li.push(("ms_ui_tree_path".to_string(), parts.join("\\")));
	li.push(("ms_ui_tree_id".to_string(), hitem.to_string()));
}

// src\pipe.rs
/*
示例：
auto_dir 模式（先立即播放一个文件；后台再从该文件所在目录读取完整列表并更新）：
@info=auto_dir=1
D:\float\OneDrive - 8b0wdy\音乐\月光\中島みゆき-銀の龍の背に乗って.flac

指定起始位置（pos 为 1-based，pos=1 表示第一首）：
@info=pos=3
D:\a.flac
D:\b.flac
D:\c.flac
D:\d.flac

@info=play_mode=rand,volume=80
D:\float\OneDrive - 8b0wdy\音乐\月光\中島みゆき-銀の龍の背に乗って.flac
D:\float\OneDrive - 8b0wdy\音乐\月光\當山みれい-願い〜あの頃のキミへ〜.flac
D:\float\OneDrive - 8b0wdy\音乐\月光\Mísia-逢いたくていま.flac
*/

struct playlist_info {
	li: Vec<String>,
	play_mode: u8, // 0 顺序 1 随机 2 单
	volume: u8,    // 0-100
}

unsafe fn list_st(st: &str) {
	let mut play_mode = 0u8;
	let mut volume = 100u8;
	let mut pos = 0usize;
	let mut auto_dir = false;
	let mut pl_name: Option<String> = None;
	let li: Vec<String>;

	if st.starts_with('@')
	{
		if let Some((info, list)) = st.split_once('\n')
		{
			let mut info_str = info[1..].trim();
			if let Some(v) = info_str.strip_prefix("info=")
			{
				info_str = v.trim();
			}

			for n in info_str.split(',')
			{
				if let Some((k, v)) = n.split_once('=')
				{
					match k.trim()
					{
						"play_mode" =>
						{
							play_mode = match v.trim()
							{
								"rand" => 1,
								"single" => 2,
								_ => 0,
							}
						}
						"volume" => volume = v.trim().parse().unwrap_or(80),
						"pos" => pos = v.trim().parse().unwrap_or(0),
						"auto_dir" | "aotu_dir" => auto_dir = v.trim().parse::<u32>().unwrap_or(0) != 0,
						"pl_name" =>
						{
							let v = v.trim();
							if !v.is_empty()
							{
								pl_name = Some(v.to_string());
							}
						}
						_ =>
						{}
					}
				}
			}
			li = list
				.lines()
				.map(|s| s.trim().trim_matches('\0').to_string())
				.filter(|s| !s.is_empty())
				.collect();
		}
		else
		{
			return;
		}
	}
	else
	{
		li = st
			.lines()
			.map(|s| s.trim().trim_matches('\0').to_string())
			.filter(|s| !s.is_empty())
			.collect();
	}

	// Deliver to playback center
	deliver_playlist(li, play_mode, volume, pos, auto_dir, pl_name.as_deref());
}

unsafe fn deliver_playlist(files: Vec<String>, play_mode: u8, volume: u8, pos: usize, auto_dir: bool, pl_name: Option<&str>) {
	if files.is_empty()
	{
		return;
	}

	let volume = volume.min(100);

	// pos 为 1-based：pos=1 表示第一首；pos=0 也视为第一首（兼容）
	let start_idx = pos
		.saturating_sub(1)
		.min(files.len() - 1);

	// auto_dir: 先立即播放选中的单个文件，后台再从该文件所在目录加载完整列表并更新
	let (files, auto_dir_path) = if auto_dir { (vec![files[start_idx].clone()], Some(files[start_idx].clone())) } else { (files, None) };

	// 映射 play_mode: pipe -> var
	// pipe: 0=顺序, 1=随机, 2=单曲
	// var:  0=单曲, 1=随机, 2=顺序
	let mapped_mode: usize = match play_mode
	{
		0 => 2, // 顺序 -> Sequential
		1 => 1, // 随机 -> Shuffle
		2 => 0, // 单曲 -> Single
		_ => 2, // 默认顺序
	};

	// 设置播放模式和音量
	g_pl_mode.store(mapped_mode, Ordering::SeqCst);
	let vol_u32 = volume as u32;
	if g_to_volume.load(Ordering::SeqCst) != vol_u32
	{
		g_to_volume.store(vol_u32, Ordering::SeqCst);
		ui_volume_sync(vol_u32);
	}

	// 目标播放列表：缺省为“用户”(id=0)，有 pl_name 则按名称查找/创建
	let target_name = pl_name
		.map(|s| s.trim())
		.filter(|s| !s.is_empty())
		.unwrap_or(PLAYLIST_NAME_USER);
	let Some(db_pl_id) = db_get_or_create_playlist_id(Some(target_name))
	else
	{
		eprintln!("[pipe] 无法创建/获取播放列表: {}", target_name);
		return;
	};
	db_save_playlist_play_mode(db_pl_id, mapped_mode);
	// 添加到播放列表池并切换
	let li_id = db_pl_id;
	if let Some(path) = auto_dir_path.as_deref()
	{
		set_pending_track_by_path(li_id, path);
	}
	let songs = collect_playlist_song_info(&files);
	let songs_for_ui = songs.clone();
	let playlist_len = songs.len();

	// 保存播放列表到数据库
	if db_replace_playlist(db_pl_id, Some(target_name), &songs)
	{
		// 保存播放状态
		let track_idx = if auto_dir { 0 } else { start_idx };
		let track_path = songs
			.get(track_idx)
			.map(|s| s.path.as_str());
		db_save_state(db_pl_id as i64, track_idx, track_path, 0, mapped_mode, vol_u32);
	}

	{
		let mut pool = m_pl_pool.write().unwrap();
		// pool.clear(); // 清空旧列表
		pool.insert(li_id, songs);
	}

	// Update UI: refresh target list view
	ui_playlist_update(li_id, songs_for_ui);

	// 设置为第一个列表并触发变更
	g_li_id.store(li_id, Ordering::SeqCst);
	ui_sync_playlist_tabs(li_id);
	g_track.store(if auto_dir { 0 } else { start_idx }, Ordering::SeqCst);
	g_pl_is_changed.store(true, Ordering::SeqCst);
	g_to_next.store(false, Ordering::SeqCst);
	g_to_prev.store(false, Ordering::SeqCst);
	if g_ev_li_chang != 0
	{
		SetEvent(g_ev_li_chang);
	}

	// 新播放列表应立即开始播放，取消暂停状态
	resume_if_paused();

	eprintln!("[pipe] playlist: id={}, name={}, {} tracks, mode={}, volume={}", li_id, target_name, playlist_len, mapped_mode, volume);

	if let Some(path) = auto_dir_path
	{
		pool_fn(dir_to_list, pl_em::st_usize(path, li_id));
	}
}

const PIPE_NAME: [u16; 15] = [92, 92, 46, 92, 112, 105, 112, 101, 92, 58, 92, 102, 111, 103, 0]; //r"\\.\pipe\:\fog"

unsafe fn serve_receive() {
	loop
	{
		let pipe = CreateNamedPipeW(PIPE_NAME.as_ptr(), 3, 0, 255, 0, 0, 0, 0);
		ConnectNamedPipe(pipe, 0);

		let mut rs = [0; 4];

		if 0 == ReadFile(pipe, rs.as_mut_ptr(), 4, 0 as _, 0 as _)
		{
			CloseHandle(pipe);
			continue;
		};

		let sz = u32::from_le_bytes(rs);
		let le = sz as usize;
		let mut rv = Vec::with_capacity(le);
		rv.set_len(le);
		let mut len = 0u32;

		let ret = ReadFile(pipe, rv.as_mut_ptr(), sz, &mut len, 0 as _);

		WriteFile(pipe, 0u32.to_le_bytes().as_ptr(), 4, 0 as _, 0 as _);

		CloseHandle(pipe);

		if ret != 0 && sz == len
		{
			if let Ok(s) = from_utf8(&rv)
			{
				list_st(s.trim());
			}
		};
	}
}

// src\pipe_to.rs
unsafe fn pipe_cmd_handler(p: pl_em) {
	let pl_em::cmd(cmd) = p
	else
	{
		return;
	};

	// 查找 fn 字段
	let fn_val = cmd
		.iter()
		.find(|(k, _)| k == "fn")
		.map(|(_, v)| v.as_str())
		.unwrap_or("");

	match fn_val
	{
		/*
		"to_tab" =>
		{
						let tab_id_str = cmd
										.iter()
										.find(|(k, _)| k == "ms_ui_tab_id")
										.map(|(_, v)| v.as_str())
										.unwrap_or("");

						if let Ok(tab_id) = tab_id_str.parse::<usize>()
						{
										PostMessageW(UI_HTAB, 40001, tab_id, 0);
						}
		}

		"del_item" =>
		{
						// 获取 ms_ui_list_pl_id 和 ms_ui_list_item_idx
						let pl_id_str = cmd
										.iter()
										.find(|(k, _)| k == "ms_ui_list_pl_id")
										.map(|(_, v)| v.as_str())
										.unwrap_or("");

						let item_idx_str = cmd
										.iter()
										.find(|(k, _)| k == "ms_ui_list_item_idx")
										.map(|(_, v)| v.as_str())
										.unwrap_or("");

						if let (Ok(pl_id), Ok(item_idx)) = (pl_id_str.parse::<usize>(), item_idx_str.parse::<i64>())
						{
										PostMessageW(UI_HWND, 40003, pl_id, item_idx);
						};
		}
		"play_item" =>
		{
						// 获取 ms_ui_list_pl_id 和 ms_ui_list_item_idx，等效左键双击播放
						let pl_id_str = cmd
										.iter()
										.find(|(k, _)| k == "ms_ui_list_pl_id")
										.map(|(_, v)| v.as_str())
										.unwrap_or("");

						let item_idx_str = cmd
										.iter()
										.find(|(k, _)| k == "ms_ui_list_item_idx")
										.map(|(_, v)| v.as_str())
										.unwrap_or("");

						if let (Ok(pl_id), Ok(item_idx)) = (pl_id_str.parse::<usize>(), item_idx_str.parse::<i64>())
						{
										PostMessageW(UI_HWND, 40004, pl_id, item_idx);
						};
		}
		"tree_to_tab" =>
		{
						// 获取 ms_ui_tree_name 和 ms_ui_tree_path
						let tree_name = cmd
										.iter()
										.find(|(k, _)| k == "ms_ui_tree_name")
										.map(|(_, v)| v.clone())
										.unwrap_or_default();

						let tree_path = cmd
										.iter()
										.find(|(k, _)| k == "ms_ui_tree_path")
										.map(|(_, v)| v.clone())
										.unwrap_or_default();

						if !tree_name.is_empty() && !tree_path.is_empty()
						{
										ui_tree_midclick_folder_action(tree_name, tree_path);
						}
		}
		"del_tab" =>
		{
						// 获取 ms_ui_tab_id
						let tab_id_str = cmd
										.iter()
										.find(|(k, _)| k == "ms_ui_tab_id")
										.map(|(_, v)| v.as_str())
										.unwrap_or("");

						if let Ok(tab_id) = tab_id_str.parse::<usize>()
						{
										PostMessageW(UI_HTAB, 40000, tab_id, 0);
						}
		}
						*/
		_ =>
		{
			eprintln!("[pipe] 未知命令: fn={}\nfrom: {:?}", fn_val, cmd);
		}
	};
}

unsafe fn pl_del_item(pl_id: usize, mut item_li: Vec<usize>) {
	if item_li.is_empty()
	{
		return;
	}

	// Accept both asc/desc; normalize to unique desc indices.
	item_li.sort_unstable();
	item_li.dedup();
	item_li.sort_unstable_by(|a, b| b.cmp(a));

	let active_li_id = g_li_id.load(Ordering::SeqCst);
	let current_idx = g_track.load(Ordering::SeqCst);

	// 从内存池删除项（倒序避免索引偏移）
	let (songs, removed_desc) = {
		let mut pool = m_pl_pool.write().unwrap();
		let Some(songs) = pool.get_mut(&pl_id)
		else
		{
			return;
		};

		let mut removed_desc: Vec<usize> = Vec::new();
		for &idx in &item_li
		{
			if idx < songs.len()
			{
				songs.remove(idx);
				removed_desc.push(idx);
			}
		}
		(songs.clone(), removed_desc)
	};

	if removed_desc.is_empty()
	{
		return;
	}

	// 从数据库删除并重新排序（同样按倒序）
	db_delete_playlist_items_at_desc(pl_id, &removed_desc);

	let mut ui_shift_playing_to: Option<usize> = None;

	// 如果删除的是当前播放列表的项，需要调整当前播放索引
	if pl_id == active_li_id
	{
		let removed_current = removed_desc
			.iter()
			.any(|&i| i == current_idx);
		let removed_before = removed_desc
			.iter()
			.filter(|&&i| i < current_idx)
			.count();

		if removed_current
		{
			if songs.is_empty()
			{
				// 播放列表已空，停止播放
				eprintln!("[ui] 播放列表已空，停止播放");

				// 设置 g_pl_is_changed 使 should_abort_playback() 返回 true，中止当前 play_track
				g_pl_is_changed.store(true, Ordering::SeqCst);
				g_to_next.store(false, Ordering::SeqCst);
				g_to_prev.store(false, Ordering::SeqCst);

				if g_ev_pl_quit != 0
				{
					SetEvent(g_ev_pl_quit);
				}
				if g_ev_li_chang != 0
				{
					SetEvent(g_ev_li_chang);
				}

				set_player_state(PlayerState::Idle);
			}
			else
			{
				// Pick the next available item at the "same" position after removals.
				let mut new_idx = current_idx.saturating_sub(removed_before);
				if new_idx >= songs.len()
				{
					new_idx = songs.len().saturating_sub(1);
				}
				g_track.store(new_idx, Ordering::SeqCst);

				// 触发切换到当前索引的新曲目
				g_pl_is_changed.store(true, Ordering::SeqCst);
				if g_ev_pl_quit != 0
				{
					SetEvent(g_ev_pl_quit);
				}
				if g_ev_li_chang != 0
				{
					SetEvent(g_ev_li_chang);
				}
			}
		}
		else if removed_before > 0
		{
			// 删除了当前播放项之前的项，索引左移
			let new_idx = current_idx.saturating_sub(removed_before);
			g_track.store(new_idx, Ordering::SeqCst);
			ui_shift_playing_to = Some(new_idx);
		}
	}

	let ui_shift_now_playing = ui_shift_playing_to.and_then(|idx| {
		songs
			.get(idx)
			.map(|song| (idx, song.clone()))
	});

	let ui_shift_selected_to = {
		let hlist = ui_listview_for_li_id(pl_id);
		if hlist == 0
		{
			None
		}
		else
		{
			let sel = SendMessageW(hlist, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as i64) as i32;
			if sel < 0 || songs.is_empty()
			{
				None
			}
			else
			{
				let sel = sel as usize;
				let removed_before = removed_desc
					.iter()
					.filter(|&&i| i < sel)
					.count();
				let removed_current = removed_desc.iter().any(|&i| i == sel);

				let mut new_sel = sel.saturating_sub(removed_before);
				if new_sel >= songs.len()
				{
					new_sel = songs.len().saturating_sub(1);
				}

				if removed_before > 0 || removed_current || sel >= songs.len() { Some(new_sel) } else { None }
			}
		}
	};

	// 更新 UI
	ui_playlist_update(pl_id, songs);

	if let Some((idx, s)) = ui_shift_now_playing
	{
		ui_set_now_playing2(pl_id, idx, &s);
	}
	if let Some(idx) = ui_shift_selected_to
	{
		ui_playlist_select(pl_id, idx);
	}
	eprintln!("[ui] 删除播放列表项: pl_id={}, count={}", pl_id, removed_desc.len());
}

// src\pl_db.rs
// SQLite 数据库模块 - 持久化播放列表和播放状态

// 数据库路径
static mut g_pl_db_path: String = String::new();

// 全局数据库句柄
static mut HDB: i64 = 0;

#[link(name = "winsqlite3")]
unsafe extern "C" {
	fn sqlite3_open16(filename: *const u16, ppdb: *mut i64) -> i32;
	fn sqlite3_close(pdb: i64) -> i32;
	fn sqlite3_shutdown() -> i32;
	fn sqlite3_step(pstmt: i64) -> i32;
	fn sqlite3_reset(pstmt: i64) -> i32;
	fn sqlite3_finalize(pstmt: i64) -> i32;
	fn sqlite3_column_count(pstmt: i64) -> i32;
	fn sqlite3_column_int64(pstmt: i64, icol: i32) -> i64;
	fn sqlite3_column_text(pstmt: i64, icol: i32) -> *const u8;
	fn sqlite3_column_blob(pstmt: i64, icol: i32) -> *const u8;
	fn sqlite3_column_bytes(pstmt: i64, icol: i32) -> usize;
	fn sqlite3_prepare_v2(pdb: i64, sql: *const u8, nbyte: usize, ppstmt: *mut i64, pztail: i64) -> i32;
	fn sqlite3_bind_blob(pstmt: i64, idx: i32, data: *const u8, nbyte: usize, destructor: i64) -> i32;
	fn sqlite3_bind_text(pstmt: i64, idx: i32, data: *const u8, nbyte: usize, destructor: i64) -> i32;
	fn sqlite3_bind_int(pstmt: i64, idx: i32, data: i32) -> i32;
	fn sqlite3_bind_int64(pstmt: i64, idx: i32, data: i64) -> i32;
	fn sqlite3_last_insert_rowid(pdb: i64) -> i64;
	fn sqlite3_changes(pdb: i64) -> i32;

	fn sqlite3_exec(
		pdb: i64, sql: *const u8, callback: Option<extern "C" fn(*mut core::ffi::c_void, i32, *mut *mut i8, *mut *mut i8) -> i32>,
		arg: *mut core::ffi::c_void, errmsg: *mut *mut i8,
	) -> i32;
}

const SQLITE_OK: i32 = 0;
const SQLITE_ROW: i32 = 100;
const SQLITE_DONE: i32 = 101;

unsafe fn sqlite_column_string_raw(stmt: i64, col: i32) -> String {
	let ptr = sqlite3_column_text(stmt, col);
	if ptr.is_null()
	{
		return String::new();
	}
	let len = sqlite3_column_bytes(stmt, col);
	let slice = from_raw_parts(ptr, len);
	String::from_utf8_lossy(slice).into_owned()
}

unsafe fn sqlite_column_string(stmt: i64, col: i32) -> String {
	fix_mojibake_music_text(sqlite_column_string_raw(stmt, col))
}

/// 初始化数据库：打开数据库文件，创建表结构
unsafe fn db_init() {
	let db_path = to_wstring(if g_pl_db_path.is_empty() { r"D:\float\disk\history\pl.db" } else { &g_pl_db_path });

	if sqlite3_open16(db_path.as_ptr(), &mut HDB) != SQLITE_OK || HDB == 0
	{
		eprintln!("[db] 数据库打开失败: {}", g_pl_db_path);
		msg_box(&format!("数据库打开失败: {}\n程序将退出。", g_pl_db_path), "错误", MB_ICONERROR | MB_OK);
		PostMessageW(G_HWND, WM_DESTROY, 0, 0);
		return;
	}

	// 创建播放列表表
	let sql_playlist = b"CREATE TABLE IF NOT EXISTS playlist (
                id INTEGER NOT NULL,
                name TEXT NOT NULL COLLATE NOCASE,
                created_at INTEGER,
                last_path TEXT COLLATE NOCASE,
                last_progress_ms INTEGER DEFAULT 0,
                last_updated_at INTEGER,
                play_mode INTEGER DEFAULT 2,
                PRIMARY KEY (id),
                UNIQUE(name)
        );\0";

	// 创建播放列表项表
	let sql_items = b"CREATE TABLE IF NOT EXISTS playlist_items (
                playlist_id INTEGER DEFAULT 1,
                idx INTEGER,
                path TEXT COLLATE NOCASE,
                file_size INTEGER DEFAULT 0,
                title TEXT COLLATE NOCASE,
                author TEXT COLLATE NOCASE,
                album_artist TEXT COLLATE NOCASE,
                album TEXT COLLATE NOCASE,
                track_number INTEGER DEFAULT 0,
                album_track_count INTEGER DEFAULT 0,
                genres TEXT COLLATE NOCASE,
                duration_ms INTEGER DEFAULT 0,
                duration_text TEXT COLLATE NOCASE,
                codec TEXT COLLATE NOCASE,
                has_cover INTEGER DEFAULT 0,
                random_played INTEGER DEFAULT 0,
                PRIMARY KEY (playlist_id, idx)
        );\0";

	// 创建播放状态表 (只有一行，id=1)
	let sql_state = b"CREATE TABLE IF NOT EXISTS playback_state (
                id INTEGER PRIMARY KEY DEFAULT 1,
                playlist_id INTEGER DEFAULT 1,
                track_idx INTEGER DEFAULT 0,
                track_path TEXT COLLATE NOCASE,
                progress_ms INTEGER DEFAULT 0,
                play_mode INTEGER DEFAULT 2,
                volume INTEGER DEFAULT 80,
                updated_at INTEGER,
                is_playing INTEGER DEFAULT 0,
                win_visible INTEGER DEFAULT 1,
                win_rect_x INTEGER DEFAULT 0,
                win_rect_y INTEGER DEFAULT 0,
                win_rect_w INTEGER DEFAULT 0,
                win_rect_h INTEGER DEFAULT 0,
                ui_split_lr INTEGER DEFAULT 720,
                ui_split_list_log INTEGER DEFAULT 650,
                ui_split_cover_tree INTEGER DEFAULT 250,
                ui_list_col_0 INTEGER DEFAULT 70,
                ui_list_col_1 INTEGER DEFAULT 351,
                ui_list_col_2 INTEGER DEFAULT 105,
                ui_list_col_3 INTEGER DEFAULT 211,
                ui_list_col_4 INTEGER DEFAULT 263
        );\0";

	// 确保播放状态表有默认行
	let sql_init_state = b"INSERT OR IGNORE INTO playback_state (id, playlist_id) VALUES (1, 1);\0";

	sqlite3_exec(HDB, sql_playlist.as_ptr(), None, null_mut(), null_mut());
	sqlite3_exec(HDB, sql_items.as_ptr(), None, null_mut(), null_mut());
	sqlite3_exec(HDB, sql_state.as_ptr(), None, null_mut(), null_mut());
	sqlite3_exec(HDB, sql_init_state.as_ptr(), None, null_mut(), null_mut());

	// 确保内建播放列表存在（用户=1，默认=2）
	db_ensure_builtin_playlists();

	eprintln!("[db] 数据库已初始化: {}", g_pl_db_path);
}

/// 获取当前时间戳 (毫秒精度)
fn current_timestamp() -> i64 {
	unsafe { GetTickCount64() as i64 }
}

// ----------------------
// Playlists (fog.db)
// ----------------------

/// 确保内建播放列表存在（用户=1，默认=2）
unsafe fn db_ensure_builtin_playlists() {
	db_upsert_playlist_meta(PLAYLIST_ID_USER, PLAYLIST_NAME_USER);
	db_upsert_playlist_meta(PLAYLIST_ID_DEFAULT, PLAYLIST_NAME_DEFAULT);
}

/// 写入/更新播放列表元数据（只保证 id->name 存在）
unsafe fn db_upsert_playlist_meta(id: usize, name: &str) -> bool {
	let timestamp = current_timestamp();

	// 先插入（如果已存在则忽略），再更新 name（确保固定 id 的名称一致）
	let sql_insert = b"INSERT OR IGNORE INTO playlist (id, name, created_at) VALUES (?, ?, ?);\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql_insert.as_ptr(), sql_insert.len(), &mut stmt, 0) != SQLITE_OK
	{
		return false;
	}
	sqlite3_bind_int64(stmt, 1, id as i64);
	sqlite3_bind_text(stmt, 2, name.as_ptr(), name.len(), -1);
	sqlite3_bind_int64(stmt, 3, timestamp);
	sqlite3_step(stmt);
	sqlite3_finalize(stmt);

	let sql_update = b"UPDATE playlist SET name=? WHERE id=?;\0";
	if sqlite3_prepare_v2(HDB, sql_update.as_ptr(), sql_update.len(), &mut stmt, 0) != SQLITE_OK
	{
		return false;
	}
	sqlite3_bind_text(stmt, 1, name.as_ptr(), name.len(), -1);
	sqlite3_bind_int64(stmt, 2, id as i64);
	sqlite3_step(stmt);
	sqlite3_finalize(stmt);
	true
}

/// 按名称查找 playlist_id
unsafe fn db_find_playlist_id_by_name(name: &str) -> Option<usize> {
	let sql = b"SELECT id FROM playlist WHERE name=? LIMIT 1;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return None;
	}
	sqlite3_bind_text(stmt, 1, name.as_ptr(), name.len(), -1);
	let id = if sqlite3_step(stmt) == SQLITE_ROW { Some(sqlite3_column_int64(stmt, 0) as usize) } else { None };
	sqlite3_finalize(stmt);
	id
}

/// 获取下一个可用 playlist_id（>=3）
unsafe fn db_next_playlist_id() -> usize {
	let sql = b"SELECT MAX(id) FROM playlist;\0";
	let mut stmt: i64 = 0;
	let mut max_id: i64 = 2;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) == SQLITE_OK
	{
		if sqlite3_step(stmt) == SQLITE_ROW
		{
			let v = sqlite3_column_int64(stmt, 0);
			if v >= 2
			{
				max_id = v;
			}
		}
		sqlite3_finalize(stmt);
	}
	((max_id + 1).max(3)) as usize
}

fn normalize_play_mode(play_mode: usize) -> usize {
	match play_mode
	{
		0 | 1 | 2 => play_mode,
		_ => 2,
	}
}

unsafe fn db_load_playlist_play_mode(playlist_id: usize) -> usize {
	let sql = b"SELECT play_mode FROM playlist WHERE id=? LIMIT 1;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return 2;
	}
	sqlite3_bind_int64(stmt, 1, playlist_id as i64);
	let mode = if sqlite3_step(stmt) == SQLITE_ROW { sqlite3_column_int64(stmt, 0) as usize } else { 2 };
	sqlite3_finalize(stmt);
	normalize_play_mode(mode)
}

unsafe fn db_save_playlist_play_mode(playlist_id: usize, play_mode: usize) {
	let play_mode = normalize_play_mode(play_mode);

	let sql = b"UPDATE playlist SET play_mode=? WHERE id=?;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return;
	}
	sqlite3_bind_int(stmt, 1, play_mode as i32);
	sqlite3_bind_int64(stmt, 2, playlist_id as i64);
	sqlite3_step(stmt);
	sqlite3_finalize(stmt);
}

unsafe fn db_collect_playlist_random_played_map(playlist_id: usize) -> HashMap<String, i64> {
	let mut out: HashMap<String, i64> = HashMap::default();
	if HDB == 0
	{
		return out;
	}

	let sql = b"SELECT path, random_played FROM playlist_items WHERE playlist_id=? AND IFNULL(random_played,0)<>0;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return out;
	}

	sqlite3_bind_int64(stmt, 1, playlist_id as i64);
	while sqlite3_step(stmt) == SQLITE_ROW
	{
		let path = sqlite_column_string_raw(stmt, 0);
		let random_played = sqlite3_column_int64(stmt, 1);
		if !path.trim().is_empty() && random_played != 0
		{
			out.insert(normalize_path_key(&path), random_played);
		}
	}
	sqlite3_finalize(stmt);
	out
}

unsafe fn db_pick_playlist_random_unplayed(playlist_id: usize, current_idx: usize, len: usize) -> Option<usize> {
	if HDB == 0 || len <= 1
	{
		return None;
	}

	let cur = current_idx.min(len - 1);
	let sql = b"SELECT idx FROM playlist_items WHERE playlist_id=? AND idx>=0 AND idx<? AND idx<>? AND IFNULL(random_played,0)=0 ORDER BY RANDOM() LIMIT 1;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return None;
	}

	sqlite3_bind_int64(stmt, 1, playlist_id as i64);
	sqlite3_bind_int64(stmt, 2, len as i64);
	sqlite3_bind_int64(stmt, 3, cur as i64);

	let idx = if sqlite3_step(stmt) == SQLITE_ROW
	{
		let idx = sqlite3_column_int64(stmt, 0);
		if idx >= 0 { Some(idx as usize) } else { None }
	}
	else
	{
		None
	};
	sqlite3_finalize(stmt);
	idx
}

unsafe fn db_mark_playlist_random_played(playlist_id: usize, idx: usize) {
	if HDB == 0
	{
		return;
	}

	let sql = b"UPDATE playlist_items SET random_played=1 WHERE playlist_id=? AND idx=?;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return;
	}
	sqlite3_bind_int64(stmt, 1, playlist_id as i64);
	sqlite3_bind_int64(stmt, 2, idx as i64);
	sqlite3_step(stmt);
	sqlite3_finalize(stmt);
}

unsafe fn db_reset_playlist_random_played(playlist_id: usize) {
	if HDB == 0
	{
		return;
	}

	let sql = b"UPDATE playlist_items SET random_played=0 WHERE playlist_id=?;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return;
	}
	sqlite3_bind_int64(stmt, 1, playlist_id as i64);
	sqlite3_step(stmt);
	sqlite3_finalize(stmt);
}

unsafe fn db_random_next_playlist_idx(playlist_id: usize, current_idx: usize, len: usize) -> Option<usize> {
	if len <= 1
	{
		return None;
	}

	let current_idx = current_idx.min(len - 1);
	db_mark_playlist_random_played(playlist_id, current_idx);

	let mut pick = db_pick_playlist_random_unplayed(playlist_id, current_idx, len);
	if pick.is_none()
	{
		db_reset_playlist_random_played(playlist_id);
		db_mark_playlist_random_played(playlist_id, current_idx);
		pick = db_pick_playlist_random_unplayed(playlist_id, current_idx, len);
	}

	if let Some(idx) = pick
	{
		db_mark_playlist_random_played(playlist_id, idx);
	}
	pick
}

// ----------------------
// Playlist resume (fog.db)
// ----------------------

unsafe fn db_update_playlist_resume(playlist_id: usize, track_path: &str, progress_ms: u64) {
	let track_path = track_path.trim();
	if track_path.is_empty()
	{
		return;
	}

	let sql = b"UPDATE playlist SET last_path=?, last_progress_ms=?, last_updated_at=? WHERE id=?;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return;
	}

	sqlite3_bind_text(stmt, 1, track_path.as_ptr(), track_path.len(), -1);
	sqlite3_bind_int64(stmt, 2, progress_ms.min(i64::MAX as u64) as i64);
	sqlite3_bind_int64(stmt, 3, current_timestamp());
	sqlite3_bind_int64(stmt, 4, playlist_id as i64);
	sqlite3_step(stmt);
	sqlite3_finalize(stmt);
}

unsafe fn db_load_playlist_resume(playlist_id: usize) -> Option<(String, u64)> {
	let sql = b"SELECT last_path, last_progress_ms FROM playlist WHERE id=? LIMIT 1;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return None;
	}

	sqlite3_bind_int64(stmt, 1, playlist_id as i64);
	if sqlite3_step(stmt) != SQLITE_ROW
	{
		sqlite3_finalize(stmt);
		return None;
	}

	let path = sqlite_column_string_raw(stmt, 0);
	let progress_ms = sqlite3_column_int64(stmt, 1).max(0) as u64;
	sqlite3_finalize(stmt);

	if path.trim().is_empty() { None } else { Some((path, progress_ms)) }
}

/// 创建新播放列表（名称唯一），返回新 id（>=3）
unsafe fn db_create_playlist(name: &str) -> Option<usize> {
	if let Some(id) = db_find_playlist_id_by_name(name)
	{
		return Some(id);
	}

	let id = db_next_playlist_id();
	let sql = b"INSERT INTO playlist (id, name, created_at) VALUES (?, ?, ?);\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return None;
	}
	sqlite3_bind_int64(stmt, 1, id as i64);
	sqlite3_bind_text(stmt, 2, name.as_ptr(), name.len(), -1);
	sqlite3_bind_int64(stmt, 3, current_timestamp());
	let ok = sqlite3_step(stmt) == SQLITE_DONE;
	sqlite3_finalize(stmt);
	if ok { Some(id) } else { None }
}

/// 获取或创建播放列表 id：缺省为“用户”(1)；存在同名则返回该 id；否则创建新列表
unsafe fn db_get_or_create_playlist_id(name: Option<&str>) -> Option<usize> {
	let name = name
		.unwrap_or(PLAYLIST_NAME_USER)
		.trim();
	if name.is_empty() || name == PLAYLIST_NAME_USER
	{
		return Some(PLAYLIST_ID_USER);
	}
	if name == PLAYLIST_NAME_DEFAULT
	{
		return Some(PLAYLIST_ID_DEFAULT);
	}
	if let Some(id) = db_find_playlist_id_by_name(name)
	{
		return Some(id);
	}
	db_create_playlist(name)
}

/// 用完整列表替换指定播放列表的内容（可选更新名称）
unsafe fn db_replace_playlist(playlist_id: usize, name: Option<&str>, songs: &[SongInfo]) -> bool {
	// 开始事务
	sqlite3_exec(HDB, b"BEGIN TRANSACTION;\0".as_ptr(), None, null_mut(), null_mut());

	if let Some(name) = name
	{
		if !db_upsert_playlist_meta(playlist_id, name)
		{
			sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
			return false;
		}
	}

	let random_played_map = db_collect_playlist_random_played_map(playlist_id);

	// 清空旧项
	let sql_del = b"DELETE FROM playlist_items WHERE playlist_id=?;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql_del.as_ptr(), sql_del.len(), &mut stmt, 0) != SQLITE_OK
	{
		sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
		return false;
	}
	sqlite3_bind_int64(stmt, 1, playlist_id as i64);
	sqlite3_step(stmt);
	sqlite3_finalize(stmt);

	// 插入新项
	let sql_item =
                b"INSERT INTO playlist_items (playlist_id, idx, path, file_size, title, author, album_artist, album, track_number, album_track_count, genres, duration_ms, duration_text, codec, has_cover, random_played) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);\0";
	let mut stmt_item: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql_item.as_ptr(), sql_item.len(), &mut stmt_item, 0) != SQLITE_OK
	{
		sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
		return false;
	}
	for (idx, song) in songs.iter().enumerate()
	{
		sqlite3_reset(stmt_item);

		let genres = song.genres.join(";");
		let mut duration_text_buf = String::new();
		let duration_text = if !song.duration_text.is_empty()
		{
			song.duration_text.as_str()
		}
		else if song.duration_ms > 0
		{
			duration_text_buf = format_time(song.duration_ms);
			duration_text_buf.as_str()
		}
		else
		{
			""
		};
		let random_played = random_played_map
			.get(&normalize_path_key(&song.path))
			.copied()
			.unwrap_or(0);

		sqlite3_bind_int64(stmt_item, 1, playlist_id as i64);
		sqlite3_bind_int(stmt_item, 2, idx as i32);
		sqlite3_bind_text(stmt_item, 3, song.path.as_ptr(), song.path.len(), -1);
		sqlite3_bind_int64(stmt_item, 4, song.file_size.min(i64::MAX as u64) as i64);
		sqlite3_bind_text(stmt_item, 5, song.title.as_ptr(), song.title.len(), -1);
		sqlite3_bind_text(stmt_item, 6, song.author.as_ptr(), song.author.len(), -1);
		sqlite3_bind_text(stmt_item, 7, song.album_artist.as_ptr(), song.album_artist.len(), -1);
		sqlite3_bind_text(stmt_item, 8, song.album.as_ptr(), song.album.len(), -1);
		sqlite3_bind_int(stmt_item, 9, song.track_number as i32);
		sqlite3_bind_int(stmt_item, 10, song.album_track_count as i32);
		sqlite3_bind_text(stmt_item, 11, genres.as_ptr(), genres.len(), -1);
		sqlite3_bind_int64(stmt_item, 12, song.duration_ms as i64);
		sqlite3_bind_text(stmt_item, 13, duration_text.as_ptr(), duration_text.len(), -1);
		sqlite3_bind_text(stmt_item, 14, song.codec.as_ptr(), song.codec.len(), -1);
		sqlite3_bind_int(stmt_item, 15, if song.has_cover { 1 } else { 0 });
		sqlite3_bind_int64(stmt_item, 16, random_played);

		if sqlite3_step(stmt_item) != SQLITE_DONE
		{
			sqlite3_finalize(stmt_item);
			sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
			return false;
		}
	}
	sqlite3_finalize(stmt_item);

	// 提交事务
	sqlite3_exec(HDB, b"COMMIT;\0".as_ptr(), None, null_mut(), null_mut());
	true
}

/// 读取所有播放列表 (id, name)，按 id 升序
/// 追加条目到指定播放列表末尾，返回追加起始 idx (0-based)
/// åˆ é™¤æ’­æ”¾åˆ—è¡¨ï¼ˆä¸ å… è®¸åˆ é™¤å†…å»º â€œç”¨æˆ·/é»˜è®¤â€?ï¼‰
unsafe fn db_delete_playlist(playlist_id: usize) -> bool {
	if playlist_id == PLAYLIST_ID_USER || playlist_id == PLAYLIST_ID_DEFAULT
	{
		return false;
	}

	db_ensure_builtin_playlists();

	sqlite3_exec(HDB, b"BEGIN TRANSACTION;\0".as_ptr(), None, null_mut(), null_mut());

	let mut stmt: i64 = 0;
	let sql_del_items = b"DELETE FROM playlist_items WHERE playlist_id=?;\0";
	if sqlite3_prepare_v2(HDB, sql_del_items.as_ptr(), sql_del_items.len(), &mut stmt, 0) != SQLITE_OK
	{
		sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
		return false;
	}
	sqlite3_bind_int64(stmt, 1, playlist_id as i64);
	sqlite3_step(stmt);
	sqlite3_finalize(stmt);

	let sql_del_playlist = b"DELETE FROM playlist WHERE id=?;\0";
	if sqlite3_prepare_v2(HDB, sql_del_playlist.as_ptr(), sql_del_playlist.len(), &mut stmt, 0) != SQLITE_OK
	{
		sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
		return false;
	}
	sqlite3_bind_int64(stmt, 1, playlist_id as i64);
	sqlite3_step(stmt);
	sqlite3_finalize(stmt);

	sqlite3_exec(HDB, b"COMMIT;\0".as_ptr(), None, null_mut(), null_mut());
	true
}

/// 删除播放列表中指定索引的项，并重新排序后续项
unsafe fn db_delete_playlist_item_at(playlist_id: usize, item_idx: usize) -> bool {
	sqlite3_exec(HDB, b"BEGIN TRANSACTION;\0".as_ptr(), None, null_mut(), null_mut());

	// 删除指定索引的项
	let sql_del = b"DELETE FROM playlist_items WHERE playlist_id=? AND idx=?;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql_del.as_ptr(), sql_del.len(), &mut stmt, 0) != SQLITE_OK
	{
		sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
		return false;
	}
	sqlite3_bind_int64(stmt, 1, playlist_id as i64);
	sqlite3_bind_int(stmt, 2, item_idx as i32);
	sqlite3_step(stmt);
	sqlite3_finalize(stmt);

	// 更新后续项的索引 (idx = idx - 1 WHERE idx > item_idx)
	let sql_update = b"UPDATE playlist_items SET idx = idx - 1 WHERE playlist_id=? AND idx > ?;\0";
	if sqlite3_prepare_v2(HDB, sql_update.as_ptr(), sql_update.len(), &mut stmt, 0) != SQLITE_OK
	{
		sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
		return false;
	}
	sqlite3_bind_int64(stmt, 1, playlist_id as i64);
	sqlite3_bind_int(stmt, 2, item_idx as i32);
	sqlite3_step(stmt);
	sqlite3_finalize(stmt);

	sqlite3_exec(HDB, b"COMMIT;\0".as_ptr(), None, null_mut(), null_mut());
	true
}

/// 查询：按 path（不区分大小写）找出需要删除的 (playlist_id, idx) 列表，按 playlist_id 升序、idx 降序。
unsafe fn db_collect_playlist_delete_plan_by_path(path: &str) -> Vec<(usize, Vec<usize>)> {
	let path = path.trim();
	if path.is_empty() || HDB == 0
	{
		return Vec::new();
	}

	let sql = b"SELECT playlist_id, idx FROM playlist_items WHERE path COLLATE NOCASE = ? ORDER BY playlist_id, idx DESC;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return Vec::new();
	}
	sqlite3_bind_text(stmt, 1, path.as_ptr(), path.len(), -1);

	let mut out: Vec<(usize, Vec<usize>)> = Vec::new();
	let mut last_pid: Option<usize> = None;
	while sqlite3_step(stmt) == SQLITE_ROW
	{
		let pid = sqlite3_column_int64(stmt, 0) as usize;
		let idx = sqlite3_column_int64(stmt, 1) as usize;

		match last_pid
		{
			Some(v) if v == pid =>
			{
				if let Some(last) = out.last_mut()
				{
					last.1.push(idx);
				}
			}
			_ =>
			{
				out.push((pid, vec![idx]));
				last_pid = Some(pid);
			}
		}
	}

	sqlite3_finalize(stmt);
	out
}

/// 删除指定播放列表内多个 idx（要求 idxs_desc 为降序），并按原逻辑重排后续 idx。
unsafe fn db_delete_playlist_items_at_desc(playlist_id: usize, idxs_desc: &[usize]) -> bool {
	if idxs_desc.is_empty()
	{
		return true;
	}

	sqlite3_exec(HDB, b"BEGIN TRANSACTION;\0".as_ptr(), None, null_mut(), null_mut());

	let sql_del = b"DELETE FROM playlist_items WHERE playlist_id=? AND idx=?;\0";
	let mut stmt_del: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql_del.as_ptr(), sql_del.len(), &mut stmt_del, 0) != SQLITE_OK
	{
		sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
		return false;
	}

	let sql_update = b"UPDATE playlist_items SET idx = idx - 1 WHERE playlist_id=? AND idx > ?;\0";
	let mut stmt_update: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql_update.as_ptr(), sql_update.len(), &mut stmt_update, 0) != SQLITE_OK
	{
		sqlite3_finalize(stmt_del);
		sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
		return false;
	}

	sqlite3_bind_int64(stmt_del, 1, playlist_id as i64);
	sqlite3_bind_int64(stmt_update, 1, playlist_id as i64);

	for &item_idx in idxs_desc
	{
		// 删除指定索引的项
		sqlite3_reset(stmt_del);
		sqlite3_bind_int(stmt_del, 2, item_idx as i32);
		sqlite3_step(stmt_del);

		// 更新后续项的索引 (idx = idx - 1 WHERE idx > item_idx)
		sqlite3_reset(stmt_update);
		sqlite3_bind_int(stmt_update, 2, item_idx as i32);
		sqlite3_step(stmt_update);
	}

	sqlite3_finalize(stmt_del);
	sqlite3_finalize(stmt_update);

	sqlite3_exec(HDB, b"COMMIT;\0".as_ptr(), None, null_mut(), null_mut());
	true
}

unsafe fn db_append_playlist_items(playlist_id: usize, songs: &[SongInfo]) -> Option<usize> {
	if songs.is_empty()
	{
		return None;
	}

	db_ensure_builtin_playlists();

	// 寮€濮嬩簨鍔?
	sqlite3_exec(HDB, b"BEGIN TRANSACTION;\0".as_ptr(), None, null_mut(), null_mut());

	// 计算起始 idx：IFNULL(MAX(idx), -1) + 1
	let sql_max = b"SELECT IFNULL(MAX(idx), -1) FROM playlist_items WHERE playlist_id=?;\0";
	let mut stmt_max: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql_max.as_ptr(), sql_max.len(), &mut stmt_max, 0) != SQLITE_OK
	{
		sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
		return None;
	}
	sqlite3_bind_int64(stmt_max, 1, playlist_id as i64);
	let mut start_idx: i64 = 0;
	if sqlite3_step(stmt_max) == SQLITE_ROW
	{
		start_idx = sqlite3_column_int64(stmt_max, 0) + 1;
	}
	sqlite3_finalize(stmt_max);
	if start_idx < 0
	{
		start_idx = 0;
	}

	let sql_item =
                b"INSERT INTO playlist_items (playlist_id, idx, path, file_size, title, author, album_artist, album, track_number, album_track_count, genres, duration_ms, duration_text, codec, has_cover) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);\0";
	let mut stmt_item: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql_item.as_ptr(), sql_item.len(), &mut stmt_item, 0) != SQLITE_OK
	{
		sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
		return None;
	}

	for (i, song) in songs.iter().enumerate()
	{
		sqlite3_reset(stmt_item);
		let idx = start_idx as usize + i;
		let genres = song.genres.join(";");
		let mut duration_text_buf = String::new();
		let duration_text = if !song.duration_text.is_empty()
		{
			song.duration_text.as_str()
		}
		else if song.duration_ms > 0
		{
			duration_text_buf = format_time(song.duration_ms);
			duration_text_buf.as_str()
		}
		else
		{
			""
		};
		sqlite3_bind_int64(stmt_item, 1, playlist_id as i64);
		sqlite3_bind_int(stmt_item, 2, idx as i32);
		sqlite3_bind_text(stmt_item, 3, song.path.as_ptr(), song.path.len(), -1);
		sqlite3_bind_int64(stmt_item, 4, song.file_size.min(i64::MAX as u64) as i64);
		sqlite3_bind_text(stmt_item, 5, song.title.as_ptr(), song.title.len(), -1);
		sqlite3_bind_text(stmt_item, 6, song.author.as_ptr(), song.author.len(), -1);
		sqlite3_bind_text(stmt_item, 7, song.album_artist.as_ptr(), song.album_artist.len(), -1);
		sqlite3_bind_text(stmt_item, 8, song.album.as_ptr(), song.album.len(), -1);
		sqlite3_bind_int(stmt_item, 9, song.track_number as i32);
		sqlite3_bind_int(stmt_item, 10, song.album_track_count as i32);
		sqlite3_bind_text(stmt_item, 11, genres.as_ptr(), genres.len(), -1);
		sqlite3_bind_int64(stmt_item, 12, song.duration_ms as i64);
		sqlite3_bind_text(stmt_item, 13, duration_text.as_ptr(), duration_text.len(), -1);
		sqlite3_bind_text(stmt_item, 14, song.codec.as_ptr(), song.codec.len(), -1);
		sqlite3_bind_int(stmt_item, 15, if song.has_cover { 1 } else { 0 });

		if sqlite3_step(stmt_item) != SQLITE_DONE
		{
			sqlite3_finalize(stmt_item);
			sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
			return None;
		}
	}
	sqlite3_finalize(stmt_item);

	sqlite3_exec(HDB, b"COMMIT;\0".as_ptr(), None, null_mut(), null_mut());
	Some(start_idx as usize)
}

unsafe fn db_load_playlists() -> Vec<(usize, String)> {
	db_ensure_builtin_playlists();
	let sql = b"SELECT id, name FROM playlist ORDER BY id;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return Vec::new();
	}

	let mut out: Vec<(usize, String)> = Vec::new();
	while sqlite3_step(stmt) == SQLITE_ROW
	{
		let id = sqlite3_column_int64(stmt, 0) as usize;
		let name = sqlite_column_string(stmt, 1);
		out.push((id, name));
	}
	sqlite3_finalize(stmt);
	out
}

/// 读取指定播放列表的条目
unsafe fn db_load_playlist_items(playlist_id: usize) -> Vec<SongInfo> {
	let sql_items =
                b"SELECT path, file_size, title, author, album_artist, album, track_number, album_track_count, genres, duration_ms, duration_text, codec, has_cover FROM playlist_items WHERE playlist_id=? ORDER BY idx;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql_items.as_ptr(), sql_items.len(), &mut stmt, 0) != SQLITE_OK
	{
		return Vec::new();
	}
	sqlite3_bind_int64(stmt, 1, playlist_id as i64);

	let mut songs: Vec<SongInfo> = Vec::new();
	while sqlite3_step(stmt) == SQLITE_ROW
	{
		let path = sqlite_column_string_raw(stmt, 0);
		let file_size = sqlite3_column_int64(stmt, 1).max(0) as u64;
		if path.is_empty()
		{
			continue;
		}

		let title = sqlite_column_string(stmt, 2);
		let author = sqlite_column_string(stmt, 3);
		let album_artist = sqlite_column_string(stmt, 4);
		let album = sqlite_column_string(stmt, 5);
		let track_number = sqlite3_column_int64(stmt, 6) as u32;
		let album_track_count = sqlite3_column_int64(stmt, 7) as u32;
		let genres_text = sqlite_column_string(stmt, 8);
		let duration_ms = sqlite3_column_int64(stmt, 9) as u64;
		let duration_text = sqlite_column_string(stmt, 10);
		let codec = sqlite_column_string(stmt, 11);
		let has_cover = sqlite3_column_int64(stmt, 12) != 0;

		let mut song = SongInfo { path, ..Default::default() };
		song.file_size = file_size;
		song.title = title;
		song.author = author;
		song.album_artist = album_artist;
		song.album = album;
		song.track_number = track_number;
		song.album_track_count = album_track_count;
		song.genres = if genres_text.is_empty() { Vec::new() } else { split_genres(&genres_text) };
		song.duration_ms = duration_ms;
		song.duration_text = if !duration_text.is_empty() && duration_ms > 0
		{
			duration_text
		}
		else if duration_ms > 0
		{
			format_time(duration_ms)
		}
		else
		{
			String::new()
		};
		song.codec = codec;
		song.has_cover = has_cover;

		if song.album_artist.is_empty() && !song.author.is_empty()
		{
			song.album_artist = song.author.clone();
		}

		songs.push(song);
	}
	sqlite3_finalize(stmt);
	songs
}

/// 保存播放列表到数据库
/// 返回新创建的 playlist_id
unsafe fn db_save_playlist(songs: &[SongInfo], name: Option<&str>) -> Option<i64> {
	if songs.is_empty()
	{
		return None;
	}

	let playlist_id = db_next_playlist_id() as i64;
	let timestamp = current_timestamp();
	let playlist_name = match name
		.map(|s| s.trim())
		.filter(|s| !s.is_empty())
	{
		Some(v) => v.to_string(),
		None => format!("playlist {}", playlist_id),
	};

	// 开始事务
	sqlite3_exec(HDB, b"BEGIN TRANSACTION;\0".as_ptr(), None, null_mut(), null_mut());

	// 插入播放列表
	let sql_insert = b"INSERT INTO playlist (id, name, created_at) VALUES (?, ?, ?);\0";
	let mut stmt: i64 = 0;

	if sqlite3_prepare_v2(HDB, sql_insert.as_ptr(), sql_insert.len(), &mut stmt, 0) != SQLITE_OK
	{
		sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
		return None;
	}

	sqlite3_bind_int64(stmt, 1, playlist_id);
	sqlite3_bind_text(stmt, 2, playlist_name.as_ptr(), playlist_name.len(), -1);
	sqlite3_bind_int64(stmt, 3, timestamp);
	sqlite3_step(stmt);
	sqlite3_finalize(stmt);

	let files = songs;

	// 插入播放列表项
	let sql_item2 =
                b"INSERT INTO playlist_items (playlist_id, idx, path, file_size, title, author, album_artist, album, track_number, album_track_count, genres, duration_ms, duration_text, codec, has_cover) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);\0";

	let mut stmt_item: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql_item2.as_ptr(), sql_item2.len(), &mut stmt_item, 0) != SQLITE_OK
	{
		sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
		return None;
	}

	for (idx, song) in songs.iter().enumerate()
	{
		sqlite3_reset(stmt_item);

		let genres = song.genres.join(";");
		let mut duration_text_buf = String::new();
		let duration_text = if !song.duration_text.is_empty()
		{
			song.duration_text.as_str()
		}
		else if song.duration_ms > 0
		{
			duration_text_buf = format_time(song.duration_ms);
			duration_text_buf.as_str()
		}
		else
		{
			""
		};
		sqlite3_bind_int64(stmt_item, 1, playlist_id);
		sqlite3_bind_int(stmt_item, 2, idx as i32);
		sqlite3_bind_text(stmt_item, 3, song.path.as_ptr(), song.path.len(), -1);
		sqlite3_bind_int64(stmt_item, 4, song.file_size.min(i64::MAX as u64) as i64);
		sqlite3_bind_text(stmt_item, 5, song.title.as_ptr(), song.title.len(), -1);
		sqlite3_bind_text(stmt_item, 6, song.author.as_ptr(), song.author.len(), -1);
		sqlite3_bind_text(stmt_item, 7, song.album_artist.as_ptr(), song.album_artist.len(), -1);
		sqlite3_bind_text(stmt_item, 8, song.album.as_ptr(), song.album.len(), -1);
		sqlite3_bind_int(stmt_item, 9, song.track_number as i32);
		sqlite3_bind_int(stmt_item, 10, song.album_track_count as i32);
		sqlite3_bind_text(stmt_item, 11, genres.as_ptr(), genres.len(), -1);
		sqlite3_bind_int64(stmt_item, 12, song.duration_ms as i64);
		sqlite3_bind_text(stmt_item, 13, duration_text.as_ptr(), duration_text.len(), -1);
		sqlite3_bind_text(stmt_item, 14, song.codec.as_ptr(), song.codec.len(), -1);
		sqlite3_bind_int(stmt_item, 15, if song.has_cover { 1 } else { 0 });

		if sqlite3_step(stmt_item) != SQLITE_DONE
		{
			sqlite3_finalize(stmt_item);
			sqlite3_exec(HDB, b"ROLLBACK;\0".as_ptr(), None, null_mut(), null_mut());
			return None;
		}
	}
	sqlite3_finalize(stmt_item);

	// 提交事务
	sqlite3_exec(HDB, b"COMMIT;\0".as_ptr(), None, null_mut(), null_mut());

	eprintln!("[db] 保存播放列表: id={}, {} 首", playlist_id, files.len());

	Some(playlist_id)
}

/// 保存当前播放状态 (不更新窗口状态，窗口状态保持原值)
/// 为了方便，我们这里只更新播放相关的字段，窗口字段用 COALESCE 保持不变
unsafe fn db_save_state(playlist_id: i64, track_idx: usize, track_path: Option<&str>, progress_ms: u64, play_mode: usize, volume: u32) {
	// 仅更新播放相关字段，updated_at 更新
	let sql = b"UPDATE playback_state SET playlist_id=?, track_idx=?, track_path=?, progress_ms=?, play_mode=?, volume=?, updated_at=? WHERE id=1;\0";
	let mut stmt: i64 = 0;

	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) == SQLITE_OK
	{
		sqlite3_bind_int64(stmt, 1, playlist_id);
		sqlite3_bind_int(stmt, 2, track_idx as i32);
		let track_path = track_path.unwrap_or("").trim();
		sqlite3_bind_text(stmt, 3, track_path.as_ptr(), track_path.len(), -1);
		sqlite3_bind_int64(stmt, 4, progress_ms.min(i64::MAX as u64) as i64);
		sqlite3_bind_int(stmt, 5, play_mode as i32);
		sqlite3_bind_int(stmt, 6, volume as i32);
		sqlite3_bind_int64(stmt, 7, current_timestamp());
		sqlite3_step(stmt);
		sqlite3_finalize(stmt);
	}

	if playlist_id > 0
	{
		let track_path = track_path.unwrap_or("").trim();
		if !track_path.is_empty()
		{
			db_update_playlist_resume(playlist_id as usize, track_path, progress_ms);
		}
	}
}

unsafe fn db_save_volume(volume: u32) {
	let sql = b"UPDATE playback_state SET volume=?, updated_at=? WHERE id=1;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) == SQLITE_OK
	{
		sqlite3_bind_int(stmt, 1, volume as i32);
		sqlite3_bind_int64(stmt, 2, current_timestamp());
		sqlite3_step(stmt);
		sqlite3_finalize(stmt);
	}
}

unsafe fn db_save_play_mode(play_mode: usize) {
	let play_mode = match play_mode
	{
		0 | 1 | 2 => play_mode,
		_ => 2,
	};

	let sql = b"UPDATE playback_state SET play_mode=?, updated_at=? WHERE id=1;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) == SQLITE_OK
	{
		sqlite3_bind_int(stmt, 1, play_mode as i32);
		sqlite3_bind_int64(stmt, 2, current_timestamp());
		sqlite3_step(stmt);
		sqlite3_finalize(stmt);
	}
}

/// 保存窗口状态
unsafe fn db_save_window_state(visible: bool, x: i32, y: i32, w: i32, h: i32) {
	let sql =
                b"UPDATE playback_state SET win_visible=?, win_rect_x=?, win_rect_y=?, win_rect_w=?, win_rect_h=?, ui_split_lr=?, ui_split_list_log=?, ui_split_cover_tree=?, updated_at=? WHERE id=1;\0";
	let mut stmt: i64 = 0;

	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) == SQLITE_OK
	{
		sqlite3_bind_int(stmt, 1, if visible { 1 } else { 0 });
		sqlite3_bind_int(stmt, 2, x);
		sqlite3_bind_int(stmt, 3, y);
		sqlite3_bind_int(stmt, 4, w);
		sqlite3_bind_int(stmt, 5, h);
		sqlite3_bind_int(
			stmt,
			6,
			UI_SPLIT_LR
				.load(Ordering::SeqCst)
				.clamp(0, 1000) as i32,
		);
		sqlite3_bind_int(
			stmt,
			7,
			UI_SPLIT_LIST_LOG
				.load(Ordering::SeqCst)
				.clamp(0, 1000) as i32,
		);
		sqlite3_bind_int(
			stmt,
			8,
			UI_SPLIT_COVER_TREE
				.load(Ordering::SeqCst)
				.clamp(0, 1000) as i32,
		);
		sqlite3_bind_int64(stmt, 9, current_timestamp());
		sqlite3_step(stmt);
		sqlite3_finalize(stmt);
	}
}

/// 保存播放列表列宽比例
unsafe fn db_save_playlist_column_ratios(ratios: &[u32; UI_PLAYLIST_COL_COUNT]) {
	let sql =
                b"UPDATE playback_state SET ui_list_col_0=?, ui_list_col_1=?, ui_list_col_2=?, ui_list_col_3=?, ui_list_col_4=?, updated_at=? WHERE id=1;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) == SQLITE_OK
	{
		sqlite3_bind_int(stmt, 1, ratios[0] as i32);
		sqlite3_bind_int(stmt, 2, ratios[1] as i32);
		sqlite3_bind_int(stmt, 3, ratios[2] as i32);
		sqlite3_bind_int(stmt, 4, ratios[3] as i32);
		sqlite3_bind_int(stmt, 5, ratios[4] as i32);
		sqlite3_bind_int64(stmt, 6, current_timestamp());
		sqlite3_step(stmt);
		sqlite3_finalize(stmt);
	}
}

/// UI 配置（与播放列表无关）
struct UiConfig {
	volume: u32,
	win_visible: bool,
	win_rect: Option<[i32; 4]>, // x, y, w, h
	ui_split_lr: u32,
	ui_split_list_log: u32,
	ui_split_cover_tree: u32,
	ui_list_col_ratios: [u32; UI_PLAYLIST_COL_COUNT],
}

/// 从数据库恢复 UI 配置（即使没有可恢复的播放列表，也应可用）
unsafe fn db_restore_ui_config() -> Option<UiConfig> {
	let sql = b"SELECT volume, win_visible, win_rect_x, win_rect_y, win_rect_w, win_rect_h, ui_split_lr, ui_split_list_log, ui_split_cover_tree, ui_list_col_0, ui_list_col_1, ui_list_col_2, ui_list_col_3, ui_list_col_4 FROM playback_state WHERE id=1;\0";
	let mut stmt: i64 = 0;

	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		return None;
	}

	if sqlite3_step(stmt) != SQLITE_ROW
	{
		sqlite3_finalize(stmt);
		return None;
	}

	let volume = sqlite3_column_int64(stmt, 0) as u32;
	let win_visible = sqlite3_column_int64(stmt, 1) != 0;
	let win_x = sqlite3_column_int64(stmt, 2) as i32;
	let win_y = sqlite3_column_int64(stmt, 3) as i32;
	let win_w = sqlite3_column_int64(stmt, 4) as i32;
	let win_h = sqlite3_column_int64(stmt, 5) as i32;
	let ui_split_lr = sqlite3_column_int64(stmt, 6).clamp(0, 1000) as u32;
	let ui_split_list_log = sqlite3_column_int64(stmt, 7).clamp(0, 1000) as u32;
	let ui_split_cover_tree = sqlite3_column_int64(stmt, 8).clamp(0, 1000) as u32;
	let ui_list_col_ratios = [
		sqlite3_column_int64(stmt, 9).clamp(0, 1000) as u32,
		sqlite3_column_int64(stmt, 10).clamp(0, 1000) as u32,
		sqlite3_column_int64(stmt, 11).clamp(0, 1000) as u32,
		sqlite3_column_int64(stmt, 12).clamp(0, 1000) as u32,
		sqlite3_column_int64(stmt, 13).clamp(0, 1000) as u32,
	];

	sqlite3_finalize(stmt);

	let win_rect = if win_w > 0 && win_h > 0 { Some([win_x, win_y, win_w, win_h]) } else { None };

	Some(UiConfig { volume, win_visible, win_rect, ui_split_lr, ui_split_list_log, ui_split_cover_tree, ui_list_col_ratios })
}

/// 保存播放暂停状态
unsafe fn db_save_playing_status(is_playing: bool) {
	let sql = b"UPDATE playback_state SET is_playing=?, updated_at=? WHERE id=1;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) == SQLITE_OK
	{
		sqlite3_bind_int(stmt, 1, if is_playing { 1 } else { 0 });
		sqlite3_bind_int64(stmt, 2, current_timestamp());
		sqlite3_step(stmt);
		sqlite3_finalize(stmt);
	}
}

/// 播放状态结构
struct PlaybackState {
	playlist_id: i64,
	track_idx: usize,
	track_path: String,
	progress_ms: u64,
	play_mode: usize,
	volume: u32,
	songs: Vec<SongInfo>,
	// 新增字段
	is_playing: bool,
	win_visible: bool,
	win_rect: Option<[i32; 4]>, // x, y, w, h
}

/// 从数据库恢复播放状态
/// 返回 (playlist_id, track_idx, progress_ms, play_mode, volume, files)
unsafe fn db_restore_state() -> Option<PlaybackState> {
	// 读取播放状态
	let sql_state =
                b"SELECT playlist_id, track_idx, track_path, progress_ms, play_mode, volume, is_playing, win_visible, win_rect_x, win_rect_y, win_rect_w, win_rect_h FROM playback_state WHERE id=1;\0";
	let mut stmt: i64 = 0;

	if sqlite3_prepare_v2(HDB, sql_state.as_ptr(), sql_state.len(), &mut stmt, 0) != SQLITE_OK
	{
		return None;
	}

	if sqlite3_step(stmt) != SQLITE_ROW
	{
		sqlite3_finalize(stmt);
		return None;
	}

	let playlist_id = sqlite3_column_int64(stmt, 0);
	let track_idx = sqlite3_column_int64(stmt, 1) as usize;
	let track_path = sqlite_column_string_raw(stmt, 2);
	let progress_ms = sqlite3_column_int64(stmt, 3).max(0) as u64;
	let play_mode = sqlite3_column_int64(stmt, 4) as usize;
	let volume = sqlite3_column_int64(stmt, 5) as u32;
	let is_playing = sqlite3_column_int64(stmt, 6) != 0;
	let win_visible = sqlite3_column_int64(stmt, 7) != 0;
	let win_x = sqlite3_column_int64(stmt, 8) as i32;
	let win_y = sqlite3_column_int64(stmt, 9) as i32;
	let win_w = sqlite3_column_int64(stmt, 10) as i32;
	let win_h = sqlite3_column_int64(stmt, 11) as i32;

	sqlite3_finalize(stmt);

	let playlist_id = if playlist_id < PLAYLIST_ID_USER as i64 { PLAYLIST_ID_USER as i64 } else { playlist_id };

	if playlist_id < 0
	{
		return None;
	}

	// 读取播放列表项
	let sql_items =
                b"SELECT path, file_size, title, author, album_artist, album, track_number, album_track_count, genres, duration_ms, duration_text, codec, has_cover FROM playlist_items WHERE playlist_id=? ORDER BY idx;\0";

	if sqlite3_prepare_v2(HDB, sql_items.as_ptr(), sql_items.len(), &mut stmt, 0) != SQLITE_OK
	{
		return None;
	}

	sqlite3_bind_int64(stmt, 1, playlist_id);

	let mut songs: Vec<SongInfo> = Vec::new();
	while sqlite3_step(stmt) == SQLITE_ROW
	{
		let path = sqlite_column_string_raw(stmt, 0);
		let file_size = sqlite3_column_int64(stmt, 1).max(0) as u64;
		if path.is_empty()
		{
			continue;
		}

		let title = sqlite_column_string(stmt, 2);
		let author = sqlite_column_string(stmt, 3);
		let album_artist = sqlite_column_string(stmt, 4);
		let album = sqlite_column_string(stmt, 5);
		let track_number = sqlite3_column_int64(stmt, 6) as u32;
		let album_track_count = sqlite3_column_int64(stmt, 7) as u32;
		let genres_text = sqlite_column_string(stmt, 8);
		let duration_ms = sqlite3_column_int64(stmt, 9) as u64;
		let duration_text = sqlite_column_string(stmt, 10);
		let codec = sqlite_column_string(stmt, 11);
		let has_cover = sqlite3_column_int64(stmt, 12) != 0;

		let mut song = SongInfo { path, ..Default::default() };
		song.file_size = file_size;
		song.title = title;
		song.author = author;
		song.album_artist = album_artist;
		song.album = album;
		song.track_number = track_number;
		song.album_track_count = album_track_count;
		song.genres = if genres_text.is_empty() { Vec::new() } else { split_genres(&genres_text) };
		song.duration_ms = duration_ms;
		song.duration_text = if !duration_text.is_empty() && duration_ms > 0
		{
			duration_text
		}
		else if duration_ms > 0
		{
			format_time(duration_ms)
		}
		else
		{
			String::new()
		};
		song.codec = codec;
		song.has_cover = has_cover;

		if song.album_artist.is_empty() && !song.author.is_empty()
		{
			song.album_artist = song.author.clone();
		}

		songs.push(song);
	}
	sqlite3_finalize(stmt);
	let files = &songs;

	if songs.is_empty()
	{
		return None;
	}

	eprintln!(
		"[db] 恢复播放状态: playlist_id={}, track={}, progress={}ms, playing={}, win_vis={}, {} 首",
		playlist_id,
		track_idx,
		progress_ms,
		is_playing,
		win_visible,
		files.len()
	);

	let win_rect = if win_w > 0 && win_h > 0 { Some([win_x, win_y, win_w, win_h]) } else { None };

	Some(PlaybackState { playlist_id, track_idx, track_path, progress_ms, play_mode, volume, songs, is_playing, win_visible, win_rect })
}

/// 更新播放进度
unsafe fn db_update_progress(playlist_id: usize, track_idx: usize, track_path: Option<&str>, progress_ms: u64) {
	let sql = b"UPDATE playback_state SET playlist_id=?, track_idx=?, track_path=?, progress_ms=?, updated_at=? WHERE id=1;\0";
	let mut stmt: i64 = 0;

	if sqlite3_prepare_v2(HDB, sql.as_ptr(), sql.len(), &mut stmt, 0) == SQLITE_OK
	{
		sqlite3_bind_int64(stmt, 1, playlist_id as i64);
		sqlite3_bind_int(stmt, 2, track_idx as i32);
		let track_path = track_path.unwrap_or("").trim();
		sqlite3_bind_text(stmt, 3, track_path.as_ptr(), track_path.len(), -1);
		sqlite3_bind_int64(stmt, 4, progress_ms.min(i64::MAX as u64) as i64);
		sqlite3_bind_int64(stmt, 5, current_timestamp());
		sqlite3_step(stmt);
		sqlite3_finalize(stmt);
	}

	let track_path = track_path.unwrap_or("").trim();
	if !track_path.is_empty()
	{
		db_update_playlist_resume(playlist_id, track_path, progress_ms);
	}
}

/// 关闭数据库
unsafe fn db_close() {
	sqlite3_close(HDB);
}

// src\player.rs
unsafe fn get_active_playlist() -> (usize, Vec<SongInfo>) {
	loop
	{
		let id = g_li_id.load(Ordering::SeqCst);
		if let Ok(pool) = m_pl_pool.read()
			&& let Some(li) = pool.get(&id)
			&& !li.is_empty()
		{
			return (id, li.clone());
		};

		WaitForSingleObject(g_ev_li_chang, 0xFFFFFFFF);
	}
}

unsafe fn player_thread(decode_tx: mpsc::Sender<DecodeCommand>, mut consumer: ringbuf::HeapCons<f64>) {
	loop
	{
		let (li_id, playlist) = get_active_playlist();
		let len = playlist.len();

		if let Some(pending_idx) = try_resolve_pending_track_index(li_id, &playlist)
		{
			g_track.store(pending_idx, Ordering::SeqCst);
		}

		let idx = g_track.load(Ordering::SeqCst);
		if idx >= len
		{
			g_track.store(0, Ordering::SeqCst);
			continue;
		}

		// 休止状态：不自动反复尝试播放，等待外部触发（新播放列表/切歌/重试请求）
		if get_player_state() == PlayerState::Stopped && !g_pl_is_changed.load(Ordering::SeqCst)
		{
			if let Some(cmds) = take_player_commands()
			{
				for cmd in cmds
				{
					match cmd
					{
						PlayerCommand::SwitchToIndex(mut i) =>
						{
							if i >= len
							{
								i = len - 1;
							}
							g_track.store(i, Ordering::SeqCst);
							g_to_pos_ms.store(0, Ordering::SeqCst);
							g_to_next.store(false, Ordering::SeqCst);
							g_to_prev.store(false, Ordering::SeqCst);
						}
					}
				}
				g_pl_is_changed.store(true, Ordering::SeqCst); // 触发一次播放（跳出休止等待）
				continue;
			};

			if g_to_next.swap(false, Ordering::SeqCst)
			{
				let next = (idx + 1) % len;
				g_track.store(next, Ordering::SeqCst);
				g_pl_is_changed.store(true, Ordering::SeqCst); // 触发一次播放（跳出休止等待）
				continue;
			}

			if g_to_prev.swap(false, Ordering::SeqCst)
			{
				let prev = if idx == 0 { len - 1 } else { idx - 1 };
				g_track.store(prev, Ordering::SeqCst);
				g_pl_is_changed.store(true, Ordering::SeqCst); // 触发一次播放（跳出休止等待）
				continue;
			}

			let req = take_playback_retry_request();
			if req == RetryReason::Restart
			{
				g_to_pos_ms.store(0, Ordering::SeqCst);
			}

			if req == RetryReason::None
			{
				let hs = [g_ev_li_chang, g_ev_pl_quit];
				WaitForMultipleObjects(2, hs.as_ptr(), 0, 0xFFFFFFFF);
				continue;
			}
		}

		let song = playlist[idx].clone();
		let mut start_pos = g_to_pos_ms.swap(0, Ordering::SeqCst) as u64;

		// 更新 UI 选中状态
		let li_id = g_li_id.load(Ordering::SeqCst);
		NOW_PLAYING_LI_ID.store(li_id, Ordering::SeqCst);
		ui_playlist_select(li_id, idx);
		ui_set_now_playing2(li_id, idx, &song);

		// 清除 g_pl_is_changed 标志，避免首次播放时在 play_track 内被错误触发停止
		// 这个标志在 deliver_playlist 中设置，此时我们已知列表变更并即将播放
		g_pl_is_changed.store(false, Ordering::SeqCst);

		// 保存播放状态到数据库
		let active_li_id = g_li_id.load(Ordering::SeqCst);
		let mode = g_pl_mode.load(Ordering::SeqCst);
		let volume = g_to_volume.load(Ordering::SeqCst);
		db_save_state(active_li_id as i64, idx, Some(song.path.as_str()), start_pos, mode, volume);

		// 内层循环处理重放请求（模式切换、重新开始）
		loop
		{
			match play_track(&song, start_pos, &decode_tx, &mut consumer)
			{
				Some(resume_ms) =>
				{
					// 需要从指定位置重放当前曲目
					start_pos = resume_ms;

					handle_playback_retry(take_last_retry_reason());
					continue;
				}
				None =>
				{
					// 正常结束或需要切换曲目
					if take_last_retry_reason() == RetryReason::TrackOpenFailed
					{
						eprintln!("[player] 无法播放，进入休止: {}", song.path);
					}
					break;
				}
			}
		}

		// 先检查：如果外部切换了播放列表，重置索引并重新进入循环（不是播放失败）
		if g_pl_is_changed.swap(false, Ordering::SeqCst)
		{
			// Keep g_track as set by deliver_playlist / commands (auto_dir may update it before we restart).
			g_to_next.store(false, Ordering::SeqCst);
			g_to_prev.store(false, Ordering::SeqCst);
			continue;
		}

		if let Some(cmds) = take_player_commands()
		{
			let (_li_id, playlist) = get_active_playlist();
			let len = playlist.len();
			for cmd in cmds
			{
				match cmd
				{
					PlayerCommand::SwitchToIndex(mut i) =>
					{
						if i >= len
						{
							i = len - 1;
						}
						g_track.store(i, Ordering::SeqCst);
						g_to_pos_ms.store(0, Ordering::SeqCst);
						g_to_next.store(false, Ordering::SeqCst);
						g_to_prev.store(false, Ordering::SeqCst);
					}
				}
			}
			continue;
		};

		// 播放期间播放列表可能被后台线程更新（例如 auto_dir），这里重新获取一次用于切歌逻辑
		let (li_id_for_next, playlist) = get_active_playlist();
		let len = playlist.len();

		let target = normalize_path_key(&song.path);
		let idx = playlist
			.iter()
			.position(|p| normalize_path_key(&p.path) == target)
			.unwrap_or_else(|| {
				g_track
					.load(Ordering::SeqCst)
					.min(len - 1)
			});

		// 休止状态：保持当前曲目与列表，不自动切到下一首 退出
		if get_player_state() == PlayerState::Stopped
		{
			let hs = [g_ev_li_chang, g_ev_pl_quit];
			WaitForMultipleObjects(2, hs.as_ptr(), 0, 0xFFFFFFFF);
			continue;
		}

		// 处理上/下一首
		if g_to_next.swap(false, Ordering::SeqCst)
		{
			let next = (idx + 1) % len;
			g_track.store(next, Ordering::SeqCst);
			continue;
		}

		if g_to_prev.swap(false, Ordering::SeqCst)
		{
			let prev = if idx == 0 { len - 1 } else { idx - 1 };
			g_track.store(prev, Ordering::SeqCst);
			continue;
		}

		// 根据播放模式决定下一首及起始位置
		let mode = current_play_mode();
		let (next_idx, next_start) = choose_next_track(mode, li_id_for_next, idx, len);
		g_track.store(next_idx, Ordering::SeqCst);
		g_to_pos_ms.store(next_start as u32, Ordering::SeqCst);
	}
}

unsafe fn handle_playback_retry(reason: RetryReason) {
	println!("handle_playback_retry");
	match reason
	{
		RetryReason::DefaultDeviceChanged | RetryReason::DeviceInvalidated | RetryReason::StartStreamFailed =>
		{
			// 蓝牙设备开关机/断线重连时，默认输出设备可能短时间内连续变更；等待稳定后再重建，避免抖动/反复重建
			wait_for_default_render_device_settle();
		}
		RetryReason::RenderWriteFailed | RetryReason::ResumeStartStreamFailed | RetryReason::WasapiInitFailed =>
		{
			// 防止热循环占用 CPU（例如驱动短暂不可用）
			Sleep(80);
		}
		RetryReason::DeviceInUse =>
		{
			// 已在 play_track 内触发暂停（ResetEvent），这里不做额外等待
		}
		_ =>
		{
			// Restart / ModeChanged / None：无需额外处理
		}
	}
}

fn current_play_mode() -> PlayMode {
	match g_pl_mode.load(Ordering::SeqCst)
	{
		0 => PlayMode::Single,
		1 => PlayMode::Shuffle,
		2 => PlayMode::Sequential,
		_ => PlayMode::Sequential,
	}
}

fn current_random_method() -> usize {
	match g_random_method.load(Ordering::SeqCst)
	{
		RANDOM_METHOD_MEMORY => RANDOM_METHOD_MEMORY,
		RANDOM_METHOD_SQL => RANDOM_METHOD_SQL,
		_ => RANDOM_METHOD_SQL,
	}
}

fn next_rand_u64() -> u64 {
	loop
	{
		let state = RNG_STATE.load(Ordering::SeqCst);
		if state == RNG_DEFAULT_SEED
		{
			let local_addr = &state as *const u64 as u64;
			let mut seed = unsafe { GetTickCount64() }
				^ ((unsafe { GetCurrentProcessId() } as u64) << 32)
				^ local_addr
				^ RNG_DEFAULT_SEED.rotate_left(17);

			seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
			seed = (seed ^ (seed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
			seed = (seed ^ (seed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
			seed ^= seed >> 31;
			if seed == 0 || seed == RNG_DEFAULT_SEED
			{
				seed = 0xD1B5_4A32_D192_ED03;
			}

			if RNG_STATE
				.compare_exchange(state, seed, Ordering::SeqCst, Ordering::SeqCst)
				.is_ok()
			{
				continue;
			}
			continue;
		}

		let next_state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
		if RNG_STATE
			.compare_exchange(state, next_state, Ordering::SeqCst, Ordering::SeqCst)
			.is_ok()
		{
			let mut value = next_state;
			value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
			value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
			return value ^ (value >> 31);
		}
	}
}

fn random_below(limit: usize) -> usize {
	if limit <= 1
	{
		return 0;
	}

	let bound = limit as u64;
	let threshold = 0u64.wrapping_sub(bound) % bound;
	loop
	{
		let value = next_rand_u64();
		if value >= threshold
		{
			return (value % bound) as usize;
		}
	}
}

fn random_index_excluding(current_idx: usize, len: usize) -> usize {
	if len <= 1
	{
		return 0;
	}

	let cur = current_idx.min(len - 1);
	let mut pick = random_below(len - 1);
	if pick >= cur
	{
		pick += 1;
	}
	pick
}

fn random_next_track_index(playlist_id: usize, current_idx: usize, len: usize) -> usize {
	if len <= 1
	{
		return 0;
	}

	match current_random_method()
	{
		RANDOM_METHOD_SQL =>
		{
			unsafe { db_random_next_playlist_idx(playlist_id, current_idx, len) }.unwrap_or_else(|| random_index_excluding(current_idx, len))
		}
		_ => random_index_excluding(current_idx, len),
	}
}

fn choose_next_track(mode: PlayMode, playlist_id: usize, current_idx: usize, len: usize) -> (usize, u64) {
	if len == 0
	{
		return (0, 0);
	}
	match mode
	{
		PlayMode::Single => (current_idx, 0),
		PlayMode::Shuffle =>
		{
			(random_next_track_index(playlist_id, current_idx, len), 0)
		}
		PlayMode::Sequential => ((current_idx + 1) % len, 0),
	}
}

unsafe fn request_playback_retry(reason: RetryReason) -> bool {
	let new = reason as u32;
	if new == 0
	{
		return false;
	}

	loop
	{
		let cur = g_pending_retry_reason.load(Ordering::SeqCst);
		if cur != 0
		{
			let cur_reason = retry_reason_from_u32(cur);
			if retry_priority(cur_reason) >= retry_priority(reason)
			{
				return false;
			}
		}

		match g_pending_retry_reason.compare_exchange(cur, new, Ordering::SeqCst, Ordering::SeqCst)
		{
			Ok(_) =>
			{
				// 唤醒可能正在等待控制/重试事件的线程（player/play_track）

				if g_ev_pl_quit != 0
				{
					SetEvent(g_ev_pl_quit);
				}

				return cur == 0;
			}
			Err(_) => continue,
		}
	}
}

// src\pool.rs
#[derive(Clone, Debug)]
enum pl_em {
	st_usize(String, usize),
	cmd(Vec<(String, String)>),
	st(String),
	th_info(SongInfo),
	f(unsafe fn(pl_em)),
	None,
}

unsafe fn pool_fn(f: unsafe fn(pl_em), p: pl_em) {
	static ev_li: Mutex<VecDeque<(i64, i64)>> = Mutex::new(VecDeque::new());

	match { ev_li.lock().unwrap().pop_front() }
	{
		Some((ev, fp)) =>
		{
			{
				(&mut *(fp as *mut Vec<(unsafe fn(pl_em), pl_em)>)).push((f, p));
			}
			SetEvent(ev);
		}

		None =>
		{
			thread::spawn(move || {
				f(p);
				let mut vc: Vec<(unsafe fn(pl_em), pl_em)> = Vec::new();
				let pt = &mut vc as *mut Vec<(unsafe fn(pl_em), pl_em)> as i64;
				let ev = CreateEventW(0, 0, 0, 0 as _);
				loop
				{
					{
						ev_li
							.lock()
							.unwrap()
							.push_back((ev, pt));
					}

					if WaitForSingleObject(ev, 0xFFFFFFFF) == 0
					{
						let (f, p) = vc.remove(0);
						f(p);
					};
				}
			});
		}
	};
}

// src\smtc.rs
static SMTC: OnceLock<SystemMediaTransportControls> = OnceLock::new();

static NOW_PLAYING: LazyLock<RwLock<SongInfo>> = LazyLock::new(|| RwLock::new(SongInfo::default()));
static NOW_PLAYING_LI_ID: AtomicUsize = AtomicUsize::new(0);
static NOW_PLAYING_ID: AtomicU64 = AtomicU64::new(0);
static NOW_PLAYING_APPLIED_ID: AtomicU64 = AtomicU64::new(0);
static SMTC_LAST_PLAYBACK_STATUS: AtomicU8 = AtomicU8::new(u8::MAX);

fn clamp_ms_to_timespan(ms: u64) -> TimeSpan {
	const TICKS_PER_MS: u64 = 10_000;
	let ms = ms.min(i64::MAX as u64 / TICKS_PER_MS);
	TimeSpan { Duration: (ms * TICKS_PER_MS) as i64 }
}

fn smtc_set_now_playing(new_np: SongInfo) {
	{
		let mut np = NOW_PLAYING.write().unwrap();
		*np = new_np;
	}
	NOW_PLAYING_ID.fetch_add(1, Ordering::SeqCst);
	smtc_sync_now_playing_if_needed();
}

fn smtc_set_now_playing_from_song(song: &SongInfo) {
	{
		let cur = NOW_PLAYING.read().unwrap();
		if cur.path == song.path
		{
			smtc_sync_now_playing_if_needed();
			return;
		}
	}

	let mut np = song.clone();
	smtc_fill_now_playing_fallbacks(&mut np);
	smtc_set_now_playing(np);
}

fn smtc_fill_now_playing_fallbacks(np: &mut SongInfo) {
	let need_path_parse = np.author.trim().is_empty() || np.title.trim().is_empty();
	if need_path_parse
	{
		let (author, title) = parse_author_title_from_path(&np.path);
		if np.author.trim().is_empty()
		{
			np.author = author;
		}
		if np.title.trim().is_empty()
		{
			np.title = title;
		}
	}

	if np.album_artist.trim().is_empty() && !np.author.trim().is_empty()
	{
		np.album_artist = np.author.clone();
	}

	if np.album.trim().is_empty()
	{
		if let Some(name) = album_name_from_parent_dir(&np.path)
		{
			np.album = name;
		}
	}
}

fn smtc_set_now_playing_from_path(path: &str) {
	{
		let cur = NOW_PLAYING.read().unwrap();
		if cur.path == path
		{
			return;
		}
	}

	let mut tags_for_print: Vec<(String, String)> = Vec::new();
	let (mut np, source) = match symphonia_collect_tags(path)
	{
		Ok(tags) =>
		{
			for t in &tags
			{
				tags_for_print.push((tag_display_key(t), t.value.clone()));
			}
			(fill_now_playing_from_symphonia_tags(path, &tags), "symphonia")
		}
		Err(_) => match ffmpeg_collect_metadata_tags(path)
		{
			Ok(tags) =>
			{
				tags_for_print.extend(tags.clone());
				(fill_now_playing_from_ffmpeg_tags(path, &tags), "ffmpeg")
			}
			Err(_) =>
			{
				let (author, title) = parse_author_title_from_path(path);
				(SongInfo { path: path.to_string(), author, title, ..Default::default() }, "path")
			}
		},
	};

	smtc_fill_now_playing_fallbacks(&mut np);

	eprintln!("[meta] {}", path);
	eprintln!("[meta] Source: {}", source);
	if !np.author.is_empty() || !np.title.is_empty()
	{
		eprintln!(
			"[meta] NowPlaying: {}{}{}",
			if np.author.is_empty() { "" } else { &np.author },
			if np.author.is_empty() || np.title.is_empty() { "" } else { " - " },
			if np.title.is_empty() { "" } else { &np.title }
		);
	}
	if !np.album.is_empty()
	{
		eprintln!("[meta] Album: {}", np.album);
	}
	if np.track_number > 0
	{
		if np.album_track_count > 0
		{
			eprintln!("[meta] Track: {}/{}", np.track_number, np.album_track_count);
		}
		else
		{
			eprintln!("[meta] Track: {}", np.track_number);
		}
	}
	if !np.genres.is_empty()
	{
		eprintln!("[meta] Genres: {}", np.genres.join("; "));
	}
	if !tags_for_print.is_empty()
	{
		eprintln!("[meta] Tags:");
		for (k, v) in tags_for_print.iter().take(200)
		{
			eprintln!("  {} = {}", k, v);
		}
		if tags_for_print.len() > 200
		{
			eprintln!("  ... ({} tags total)", tags_for_print.len());
		}
	}

	smtc_set_now_playing(np);
}

fn smtc_apply_now_playing(np: &SongInfo) {
	let Some(smtc) = SMTC.get()
	else
	{
		return;
	};

	let updater = match smtc.DisplayUpdater()
	{
		Ok(u) => u,
		Err(_) => return,
	};

	updater
		.SetType(MediaPlaybackType::Music)
		.ok();
	if let Ok(music) = updater.MusicProperties()
	{
		music
			.SetArtist(&HSTRING::from(&np.author))
			.ok();
		music
			.SetTitle(&HSTRING::from(&np.title))
			.ok();
		if !np.album_artist.is_empty()
		{
			music
				.SetAlbumArtist(&HSTRING::from(&np.album_artist))
				.ok();
		}
		if !np.album.is_empty()
		{
			music
				.SetAlbumTitle(&HSTRING::from(&np.album))
				.ok();
		}
		if np.track_number > 0
		{
			music
				.SetTrackNumber(np.track_number)
				.ok();
		}
		if np.album_track_count > 0
		{
			music
				.SetAlbumTrackCount(np.album_track_count)
				.ok();
		}
		if !np.genres.is_empty()
		{
			if let Ok(genres) = music.Genres()
			{
				let _ = genres.Clear();
				for g in &np.genres
				{
					let _ = genres.Append(&HSTRING::from(g));
				}
			}
		}
	}
	updater.Update().ok();
}

fn smtc_sync_now_playing_if_needed() {
	if SMTC.get().is_none()
	{
		return;
	}

	let id = NOW_PLAYING_ID.load(Ordering::SeqCst);
	if id == 0
	{
		return;
	}
	if NOW_PLAYING_APPLIED_ID.load(Ordering::SeqCst) == id
	{
		return;
	}

	let np = { NOW_PLAYING.read().unwrap().clone() };
	smtc_apply_now_playing(&np);
	NOW_PLAYING_APPLIED_ID.store(id, Ordering::SeqCst);
}

fn smtc_update_timeline(position_ms: u64, duration_ms: u64) {
	let Some(smtc) = SMTC.get()
	else
	{
		return;
	};
	if duration_ms == 0
	{
		return;
	}

	let pos = position_ms.min(duration_ms);
	let Ok(timeline) = SystemMediaTransportControlsTimelineProperties::new()
	else
	{
		return;
	};
	let end = clamp_ms_to_timespan(duration_ms);

	timeline
		.SetStartTime(TimeSpan { Duration: 0 })
		.ok();
	timeline.SetEndTime(end).ok();
	timeline
		.SetMinSeekTime(TimeSpan { Duration: 0 })
		.ok();
	timeline.SetMaxSeekTime(end).ok();
	timeline
		.SetPosition(clamp_ms_to_timespan(pos))
		.ok();

	smtc.UpdateTimelineProperties(&timeline)
		.ok();
}

fn smtc_update_playback_status(state: PlayerState) {
	let Some(smtc) = SMTC.get()
	else
	{
		return;
	};

	let (status, status_value) = match state
	{
		PlayerState::Playing => (MediaPlaybackStatus::Playing, 1),
		PlayerState::Paused => (MediaPlaybackStatus::Paused, 2),
		PlayerState::Stopped | PlayerState::Idle | PlayerState::Error => (MediaPlaybackStatus::Stopped, 3),
	};

	if SMTC_LAST_PLAYBACK_STATUS.swap(status_value, Ordering::SeqCst) == status_value
	{
		return;
	}
	smtc.SetPlaybackStatus(status).ok();
}

unsafe fn init_smtc(hwnd: i64) {
	if SMTC.get().is_some()
	{
		return;
	}

	if let Err(e) = RoInitialize(RO_INIT_MULTITHREADED)
	{
		eprintln!("[smtc] RoInitialize 失败: {:?}", e);
		return;
	}

	let interop: ISystemMediaTransportControlsInterop =
		match windows::core::factory::<SystemMediaTransportControls, ISystemMediaTransportControlsInterop>()
		{
			Ok(f) => f,
			Err(e) =>
			{
				eprintln!("[smtc] 读取 SMTC Factory 失败: {:?}", e);
				return;
			}
		};

	let hwnd = HWND(hwnd as usize as _);

	let smtc: SystemMediaTransportControls = match interop.GetForWindow(hwnd)
	{
		Ok(v) => v,
		Err(e) =>
		{
			eprintln!("[smtc] GetForWindow 失败: {:?}", e);
			return;
		}
	};

	let _ = SMTC.set(smtc.clone());

	smtc.SetIsEnabled(true).ok();
	smtc.SetIsPlayEnabled(true).ok();
	smtc.SetIsPauseEnabled(true).ok();
	smtc.SetIsNextEnabled(true).ok();
	smtc.SetIsPreviousEnabled(true).ok();
	smtc.SetPlaybackStatus(MediaPlaybackStatus::Stopped)
		.ok();
	SMTC_LAST_PLAYBACK_STATUS.store(3, Ordering::SeqCst);

	let handler = TypedEventHandler::<SystemMediaTransportControls, SystemMediaTransportControlsButtonPressedEventArgs>::new(|_, args| {
		let btn = args.ok()?.Button()?;
		unsafe {
			match btn
			{
				SystemMediaTransportControlsButton::Play =>
				{
					eprintln!("[smtc] Play");
					PostMessageW(G_HWND, WM_RESUME, 0, 0);
				}
				SystemMediaTransportControlsButton::Pause =>
				{
					eprintln!("[smtc] Pause");
					PostMessageW(G_HWND, WM_PAUSE, 0, 0);
				}
				SystemMediaTransportControlsButton::Stop =>
				{
					eprintln!("[smtc] Stop");
					PostMessageW(G_HWND, WM_PAUSE, 0, 0);
				}
				SystemMediaTransportControlsButton::Next =>
				{
					eprintln!("[smtc] Next");
					PostMessageW(G_HWND, WM_NEXT_TRACK, 0, 0);
				}
				SystemMediaTransportControlsButton::Previous =>
				{
					eprintln!("[smtc] Previous");
					PostMessageW(G_HWND, WM_PREV_TRACK, 0, 0);
				}
				_ =>
				{}
			}
		}
		Ok(())
	});

	if let Err(e) = smtc.ButtonPressed(&handler)
	{
		eprintln!("[smtc] ButtonPressed 注册失败: {:?}", e);
	}
	else
	{
		eprintln!("[smtc] 已启用 (系统媒体控制)");
	}
}

// src\state.rs
// 播放状态枚举
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum PlayerState {
	Idle = 0,    // 空闲 - 无播放列表
	Playing = 1, // 播放中
	Paused = 2,  // 暂停
	Stopped = 3, // 休止 - 有播放列表但未播放
	Error = 4,   // 报错
}

unsafe fn set_player_state(state: PlayerState) {
	let state_value = state as u8;
	if g_pl_state.swap(state_value, Ordering::SeqCst) == state_value
	{
		return;
	}
	PostMessageW(G_HWND, WM_SMTC_STATUS, state as usize, 0);
}

fn get_player_state() -> PlayerState {
	match g_pl_state.load(Ordering::SeqCst)
	{
		1 => PlayerState::Playing,
		2 => PlayerState::Paused,
		3 => PlayerState::Stopped,
		4 => PlayerState::Error,
		_ => PlayerState::Idle,
	}
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum RetryReason {
	None = 0,
	Restart = 1,
	ModeChanged = 2,
	DefaultDeviceChanged = 3,
	DeviceInvalidated = 4,
	DeviceInUse = 5,
	StartStreamFailed = 6,
	ResumeStartStreamFailed = 7,
	RenderWriteFailed = 8,
	WasapiInitFailed = 9,
	PauseExclusiveRelease = 10,
	TrackOpenFailed = 11,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlayMode {
	Single = 0,
	Shuffle = 1,
	Sequential = 2,
}

// 播放线程控制命令（由 window_proc / 外部线程投递）
#[derive(Clone)]
enum PlayerCommand {
	SwitchToIndex(usize),
}

fn retry_reason_from_u32(v: u32) -> RetryReason {
	match v
	{
		1 => RetryReason::Restart,
		2 => RetryReason::ModeChanged,
		3 => RetryReason::DefaultDeviceChanged,
		4 => RetryReason::DeviceInvalidated,
		5 => RetryReason::DeviceInUse,
		6 => RetryReason::StartStreamFailed,
		7 => RetryReason::ResumeStartStreamFailed,
		8 => RetryReason::RenderWriteFailed,
		9 => RetryReason::WasapiInitFailed,
		10 => RetryReason::PauseExclusiveRelease,
		11 => RetryReason::TrackOpenFailed,
		_ => RetryReason::None,
	}
}

fn retry_priority(reason: RetryReason) -> u32 {
	match reason
	{
		RetryReason::Restart => 100,
		RetryReason::ModeChanged => 90,
		RetryReason::DeviceInvalidated => 80,
		RetryReason::StartStreamFailed => 75,
		RetryReason::DefaultDeviceChanged => 70,
		RetryReason::DeviceInUse => 60,
		RetryReason::ResumeStartStreamFailed => 50,
		RetryReason::RenderWriteFailed => 40,
		RetryReason::WasapiInitFailed => 30,
		RetryReason::PauseExclusiveRelease => 20,
		RetryReason::TrackOpenFailed => 10,
		RetryReason::None => 0,
	}
}

fn take_playback_retry_request() -> RetryReason {
	let v = g_pending_retry_reason.swap(0, Ordering::SeqCst);
	retry_reason_from_u32(v)
}

fn clear_playback_retry_request_if(reason: RetryReason) {
	let _ = g_pending_retry_reason.compare_exchange(reason as u32, 0, Ordering::SeqCst, Ordering::SeqCst);
}

fn set_last_retry_reason(reason: RetryReason) {
	LAST_RETRY_REASON.store(reason as u32, Ordering::SeqCst);
}

fn take_last_retry_reason() -> RetryReason {
	let v = LAST_RETRY_REASON.swap(0, Ordering::SeqCst);
	retry_reason_from_u32(v)
}

// src\struct.rs
#[repr(C)]
struct OVERLAPPED {
	internal: usize,
	internal_high: usize,
	pointer: usize,
	h_event: i64,
}

#[repr(C)]
struct FILE_NOTIFY_INFORMATION {
	NextEntryOffset: u32,
	Action: u32,
	FileNameLength: u32,
	FileName: [u16; 1],
}

#[repr(C)]
struct WNDCLASSW {
	style: u32,
	lpfnWndProc: unsafe extern "system" fn(i64, u32, usize, i64) -> i64,
	cbClsExtra: i32,
	cbWndExtra: i32,
	hInstance: i64,
	hIcon: i64,
	hCursor: i64,
	hbrBackground: i64,
	lpszMenuName: *const u16,
	lpszClassName: *const u16,
}

#[repr(C)]
struct MSG {
	hwnd: i64,
	message: u32,
	wParam: usize,
	lParam: i64,
	time: u32,
	pt_x: i32,
	pt_y: i32,
}

#[repr(C)]
struct GUID {
	data1: u32,
	data2: u16,
	data3: u16,
	data4: [u8; 8],
}

#[repr(C)]
struct NOTIFYICONDATAW {
	cbSize: u32,
	hWnd: i64,
	uID: u32,
	uFlags: u32,
	uCallbackMessage: u32,
	hIcon: i64,
	szTip: [u16; 128],
	dwState: u32,
	dwStateMask: u32,
	szInfo: [u16; 256],
	uTimeoutOrVersion: u32,
	szInfoTitle: [u16; 64],
	dwInfoFlags: u32,
	guidItem: GUID,
	hBalloonIcon: i64,
}

#[repr(C)]
struct INITCOMMONCONTROLSEX {
	dwSize: u32,
	dwICC: u32,
}

#[repr(C)]
struct DRAWITEMSTRUCT {
	CtlType: u32,
	CtlID: u32,
	itemID: u32,
	itemAction: u32,
	itemState: u32,
	hwndItem: i64,
	hDC: i64,
	rcItem: RECT,
	itemData: usize,
}

#[repr(C)]
struct PAINTSTRUCT {
	hdc: i64,
	fErase: i32,
	rcPaint: RECT,
	fRestore: i32,
	fIncUpdate: i32,
	rgbReserved: [u8; 32],
}

// ListView 列结构
#[repr(C)]
struct LVCOLUMNW {
	mask: u32,
	fmt: i32,
	cx: i32,
	pszText: *const u16,
	cchTextMax: i32,
	iSubItem: i32,
	iImage: i32,
	iOrder: i32,
	cxMin: i32,
	cxDefault: i32,
	cxIdeal: i32,
}

// ListView 项结构
#[repr(C)]
struct LVITEMW {
	mask: u32,
	iItem: i32,
	iSubItem: i32,
	state: u32,
	stateMask: u32,
	pszText: *const u16,
	cchTextMax: i32,
	iImage: i32,
	lParam: i64,
	iIndent: i32,
	iGroupId: i32,
	cColumns: u32,
	puColumns: *mut u32,
	piColFmt: *mut i32,
	iGroup: i32,
}

// TreeView 项结构
#[repr(C)]
struct TVITEMW {
	mask: u32,
	hItem: i64,
	state: u32,
	stateMask: u32,
	pszText: *const u16,
	cchTextMax: i32,
	iImage: i32,
	iSelectedImage: i32,
	cChildren: i32,
	lParam: i64,
}

// TreeView 插入结构
#[repr(C)]
struct TVINSERTSTRUCTW {
	hParent: i64,
	hInsertAfter: i64,
	item: TVITEMW,
}

// TreeView hit-test structure
#[repr(C)]
struct TVHITTESTINFO {
	pt: POINT,
	flags: u32,
	hItem: i64,
}

// TreeView notify (Unicode)
#[repr(C)]
struct NMTREEVIEWW {
	hdr: NMHDR,
	action: u32,
	itemOld: TVITEMW,
	itemNew: TVITEMW,
	ptDrag: POINT,
}

// WM_NOTIFY 通知头
#[repr(C)]
// TabControl item
#[repr(C)]
struct TCITEMW {
	mask: u32,
	dwState: u32,
	dwStateMask: u32,
	pszText: *const u16,
	cchTextMax: i32,
	iImage: i32,
	lParam: i64,
}

// TabControl hit-test structure
#[repr(C)]
struct TCHITTESTINFO {
	pt: POINT,
	flags: u32,
}

// ListView hit-test structure
#[repr(C)]
struct LVHITTESTINFO {
	pt: POINT,
	flags: u32,
	iItem: i32,
	iSubItem: i32,
	iGroup: i32,
}

// Header hit-test structure (HDM_HITTEST)
#[repr(C)]
struct HDHITTESTINFO {
	pt: POINT,
	flags: u32,
	iItem: i32,
}

struct NMHDR {
	hwndFrom: i64,
	idFrom: usize,
	code: i32,
}

#[repr(C)]
struct NMCUSTOMDRAW {
	hdr: NMHDR,
	dwDrawStage: u32,
	hdc: i64,
	rc: RECT,
	dwItemSpec: usize,
	uItemState: u32,
	lItemlParam: i64,
}

#[repr(C)]
struct NMLVCUSTOMDRAW {
	nmcd: NMCUSTOMDRAW,
	clrText: u32,
	clrTextBk: u32,
	iSubItem: i32,
}

// ListView 通知结构 (列点击等)
#[repr(C)]
struct NMLISTVIEW {
	hdr: NMHDR,
	iItem: i32,
	iSubItem: i32,
	uNewState: u32,
	uOldState: u32,
	uChanged: u32,
	ptAction: POINT,
	lParam: i64,
}

// ListView 通知结构 (包含点击位置)
#[repr(C)]
struct NMITEMACTIVATE {
	hdr: NMHDR,
	iItem: i32,
	iSubItem: i32,
	uNewState: u32,
	uOldState: u32,
	uChanged: u32,
	ptAction: POINT,
	lParam: i64,
	uKeyFlags: u32,
}

#[repr(C)]
struct NMLVDISPINFOW {
	hdr: NMHDR,
	item: LVITEMW,
}

#[derive(Debug)]
#[repr(C)]
struct POINT {
	x: i32,
	y: i32,
}

#[derive(Debug)]
#[repr(C)]
struct RECT {
	left: i32,
	top: i32,
	right: i32,
	bottom: i32,
}

// src\sys.rs
#[link(name = "user32")]
unsafe extern "system" {
	fn SetWindowLongPtrW(hWnd: i64, nIndex: i32, dwNewLong: i64) -> i64;
	fn CallWindowProcW(lpPrevWndFunc: i64, hWnd: i64, Msg: u32, wParam: usize, lParam: i64) -> i64;
	fn ScreenToClient(hWnd: i64, lpPoint: *mut POINT) -> i32;
	fn ShowWindow(hWnd: i64, nCmdShow: i32) -> i32;
	fn UpdateWindow(hWnd: i64) -> i32;
	fn SendMessageW(hWnd: i64, Msg: u32, wParam: usize, lParam: i64) -> i64;
	fn GetClientRect(hWnd: i64, lpRect: *mut RECT) -> i32;
	fn FillRect(hDC: i64, lprc: *const RECT, hbr: i64) -> i32;
	fn SetWindowPos(hWnd: i64, hWndInsertAfter: i64, X: i32, Y: i32, cx: i32, cy: i32, uFlags: u32) -> i32;
	fn MoveWindow(hWnd: i64, X: i32, Y: i32, nWidth: i32, nHeight: i32, bRepaint: i32) -> i32;
	fn LoadCursorW(hInstance: i64, lpCursorName: *const u16) -> i64;
	fn DestroyWindow(hWnd: i64) -> i32;
	fn SetWindowTextW(hWnd: i64, lpString: *const u16) -> i32;
	fn GetWindowLongW(hWnd: i64, nIndex: i32) -> i32;
	fn SetForegroundWindow(hWnd: i64) -> i32;
	fn SetFocus(hWnd: i64) -> i64;
	fn SetCapture(hWnd: i64) -> i64;
	fn ReleaseCapture() -> i32;
	fn SetCursor(hCursor: i64) -> i64;
	fn GetWindowRect(hWnd: i64, lpRect: *mut RECT) -> i32;
	fn BeginPaint(hWnd: i64, lpPaint: *mut PAINTSTRUCT) -> i64;
	fn EndPaint(hWnd: i64, lpPaint: *const PAINTSTRUCT) -> i32;
	fn FindWindowW(lpClassName: *const u16, lpWindowName: *const u16) -> i64;
	fn FindFirstFileW(lpFileName: *const u16, lpFindFileData: *mut WIN32_FIND_DATAW) -> i64;
	fn FindNextFileW(_: i64, _: *mut WIN32_FIND_DATAW) -> i32;
	fn FindClose(_: i64) -> i32;
	fn RegisterClassW(lpWndClass: *const WNDCLASSW) -> u16;
	fn CreateWindowExW(
		dwExStyle: u32, lpClassName: *const u16, lpWindowName: *const u16, dwStyle: u32, X: i32, Y: i32, nWidth: i32, nHeight: i32,
		hWndParent: i64, hMenu: i64, hInstance: i64, lpParam: i64,
	) -> i64;
	fn DefWindowProcW(hWnd: i64, Msg: u32, wParam: usize, lParam: i64) -> i64;
	fn RegisterWindowMessageW(lpString: *const u16) -> u32;
	fn PostQuitMessage(nExitCode: i32);
	fn GetMessageW(lpMsg: *mut MSG, hWnd: i64, wMsgFilterMin: u32, wMsgFilterMax: u32) -> i32;
	fn TranslateMessage(lpMsg: *const MSG) -> i32;
	fn DispatchMessageW(lpMsg: *const MSG) -> i64;
	fn PostMessageW(hWnd: i64, Msg: u32, wParam: usize, lParam: i64) -> i32;
	fn RegisterHotKey(hWnd: i64, id: i32, fsModifiers: u32, vk: u32) -> i32;
	fn SetWindowsHookExW(idHook: i32, lpfn: unsafe extern "system" fn(i32, usize, i64) -> i64, hMod: i64, dwThreadId: u32) -> i64;
	fn CallNextHookEx(hhk: i64, nCode: i32, wParam: usize, lParam: i64) -> i64;
	fn UnhookWindowsHookEx(hhk: i64) -> i32;
	fn LoadImageW(hInst: i64, name: *const u16, ty: u32, cx: i32, cy: i32, fuLoad: u32) -> i64;
	fn DestroyIcon(hIcon: i64) -> i32;
	fn SetTimer(hWnd: i64, nIDEvent: usize, uElapse: u32, lpTimerFunc: i64) -> usize;
	fn KillTimer(hWnd: i64, nIDEvent: usize) -> i32;
	fn MessageBoxW(hWnd: i64, lpText: *const u16, lpCaption: *const u16, uType: u32) -> i32;
	fn InvalidateRect(hWnd: i64, lpRect: *const RECT, bErase: i32) -> i32;

	fn CreateEventW(lpEventAttributes: i64, bManualReset: i32, bInitialState: i32, lpName: *const u16) -> i64;
	fn ResetEvent(_: i64) -> i32;
	fn SetEvent(_: i64) -> i32;
	fn WaitForMultipleObjects(nCount: u32, lpisizes: *const i64, bWaitAll: i32, dwMilliseconds: u32) -> u32;
	fn WaitForSingleObject(_: i64, _: u32) -> u32;
	fn MapWindowPoints(hWndFrom: i64, hWndTo: i64, lpPoints: *mut POINT, cPoints: u32) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
	fn Shell_NotifyIconW(dwMessage: u32, lpData: *mut NOTIFYICONDATAW) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
	fn LoadLibraryW(lpLibFileName: *const u16) -> i64;
	fn GetProcAddress(hModule: i64, lpProcName: *const u8) -> i64;
	fn GetModuleHandleW(lpModuleName: *const u16) -> i64;
	fn Sleep(dwMilliseconds: u32);
	fn GetTickCount() -> u32;
	fn GetTickCount64() -> u64;
	fn GetLastError() -> u32;
	fn MultiByteToWideChar(
		CodePage: u32, dwFlags: u32, lpMultiByteStr: *const u8, cbMultiByte: i32, lpWideCharStr: *mut u16, cchWideChar: i32,
	) -> i32;
	fn WideCharToMultiByte(
		CodePage: u32, dwFlags: u32, lpWideCharStr: *const u16, cchWideChar: i32, lpMultiByteStr: *mut u8, cbMultiByte: i32,
		lpDefaultChar: *const u8, lpUsedDefaultChar: *mut i32,
	) -> i32;

	fn GetFileAttributesW(lpFileName: *const u16) -> u32;

	fn GetOverlappedResult(h: i64, lpOverlapped: *mut OVERLAPPED, lpNumberOfBytesTransferred: *mut u32, bWait: i32) -> i32;

	fn CreateNamedPipeW(
		name: *const u16, openMode: u32, pipeMode: u32, maxInstances: u32, outBufferSize: u32, inBufferSize: u32, defaultTimeout: u32,
		sa: i64,
	) -> i64;
	fn ConnectNamedPipe(pipe: i64, overlapped: i64) -> i64;
	fn ReadFile(h: i64, buf: *mut u8, n: u32, read: *mut u32, ov: *mut u32) -> i32;
	fn WriteFile(h: i64, buf: *const u8, n: u32, wrote: *mut u32, ov: *mut u32) -> i32;
	fn CloseHandle(h: i64) -> i32;

	fn GetCurrentProcessId() -> u32;

	fn CreateFileW(
		lpFileName: *const u16, dwDesiredAccess: u32, dwShareMode: u32, lpSecurityAttributes: i64, dwCreationDisposition: u32,
		dwFlagsAndAttributes: u32, hTemplateFile: i64,
	) -> i64;

	fn ReadDirectoryChangesW(
		hDirectory: i64, lpBuffer: *mut u8, nBufferLength: u32, bWatchSubtree: i32, dwNotifyFilter: u32, lpBytesReturned: *mut u32,
		lpOverlapped: i64, lpCompletionRoutine: i64,
	) -> i32;
}

// MMCSS (Multimedia Class Scheduler Service) - 提升音频线程优先级
#[link(name = "avrt")]
unsafe extern "system" {
	fn AvSetMmThreadCharacteristicsW(TaskName: *const u16, TaskIndex: *mut u32) -> i64;
	fn AvRevertMmThreadCharacteristics(AvrtHandle: i64) -> i32;
}

#[link(name = "gdi32")]
unsafe extern "system" {
	fn CreateFontW(
		cHeight: i32, cWidth: i32, cEscapement: i32, cOrientation: i32, cWeight: i32, bItalic: u32, bUnderline: u32, bStrikeOut: u32,
		iCharSet: u32, iOutPrecision: u32, iClipPrecision: u32, iQuality: u32, iPitchAndFamily: u32, pszFaceName: *const u16,
	) -> i64;
	fn GetStockObject(i: i32) -> i64;
	fn CreateSolidBrush(color: u32) -> i64;
	fn DeleteObject(ho: i64) -> i32;
}
// Common Controls 初始化
#[link(name = "comctl32")]
unsafe extern "system" {
	fn InitCommonControlsEx(picce: *const INITCOMMONCONTROLSEX) -> i32;
}

#[link(name = "gdiplus")]
unsafe extern "system" {
	fn GdiplusStartup(token: *mut usize, input: *const GdiplusStartupInput, output: *mut usize) -> i32;
	fn GdiplusShutdown(token: usize);
	fn GdipCreateBitmapFromStream(stream: i64, bitmap: *mut i64) -> i32;
	fn GdipDisposeImage(image: i64) -> i32;
	fn GdipGetImageWidth(image: i64, width: *mut u32) -> i32;
	fn GdipGetImageHeight(image: i64, height: *mut u32) -> i32;
	fn GdipCreateFromHDC(hdc: i64, graphics: *mut i64) -> i32;
	fn GdipDeleteGraphics(graphics: i64) -> i32;
	fn GdipDrawImageRectI(graphics: i64, image: i64, x: i32, y: i32, width: i32, height: i32) -> i32;
	fn GdipSetInterpolationMode(graphics: i64, mode: i32) -> i32;
	fn GdipSetPixelOffsetMode(graphics: i64, mode: i32) -> i32;
	fn GdipSetSmoothingMode(graphics: i64, mode: i32) -> i32;
	fn GdipSetCompositingQuality(graphics: i64, quality: i32) -> i32;
}

#[link(name = "shlwapi")]
unsafe extern "system" {
	fn SHCreateMemStream(p_init: *const u8, cb_init: u32) -> i64;
}

#[repr(C)]
struct GdiplusStartupInput {
	gdiplus_version: u32,
	debug_event_callback: usize,
	suppress_background_thread: i32,
	suppress_external_codecs: i32,
}

// src\tags.rs
#[derive(Clone, Default, Debug)]
struct SongInfo {
	path: String,
	file_size: u64,
	codec: String,
	has_cover: bool,
	duration_ms: u64,
	duration_text: String,
	author: String,
	title: String,
	album_artist: String,
	album: String,
	track_number: u32,
	album_track_count: u32,
	genres: Vec<String>,
}

#[derive(Clone)]
struct TagItem {
	std_tag: Option<StandardTag>,
	key: String,
	value: String,
}

fn parse_author_title_from_path(path: &str) -> (String, String) {
	let stem = Path::new(path)
		.file_stem()
		.and_then(|s| s.to_str())
		.unwrap_or(path)
		.trim()
		.to_string();

	if stem.is_empty()
	{
		return ("".to_string(), path.to_string());
	}

	const SEPS: [&str; 6] = [" - ", "-", "－", "–", "—", "_"];
	for sep in SEPS
	{
		if let Some((a, b)) = stem.split_once(sep)
		{
			let a = a.trim();
			let b = b.trim();
			if !a.is_empty() && !b.is_empty()
			{
				return (a.to_string(), b.to_string());
			}
		}
	}

	("".to_string(), stem)
}

fn album_name_from_parent_dir(path: &str) -> Option<String> {
	let parent = Path::new(path).parent()?;
	let name = parent.file_name()?.to_string_lossy();
	let name = name.trim();
	if name.is_empty()
	{
		return None;
	}
	Some(name.to_string())
}

fn parse_first_u32(s: &str) -> Option<u32> {
	let s = s.trim();
	let digits: String = s
		.chars()
		.take_while(|c| c.is_ascii_digit())
		.collect();
	digits.parse().ok()
}

fn parse_u32_pair_slash(s: &str) -> (Option<u32>, Option<u32>) {
	let mut parts = s.trim().splitn(2, '/');
	let a = parts.next().and_then(parse_first_u32);
	let b = parts.next().and_then(parse_first_u32);
	(a, b)
}

fn split_genres(s: &str) -> Vec<String> {
	s.split(|c| matches!(c, ';' | ',' | '/' | '\\' | '|'))
		.map(|v| v.trim())
		.filter(|v| !v.is_empty())
		.map(|v| v.to_string())
		.collect()
}

fn symphonia_raw_value_to_string(value: &RawValue) -> Option<String> {

	match value
	{
		RawValue::String(v) => Some(v.as_ref().to_string()),
		RawValue::StringList(v) => Some(v.as_ref().join("; ")),
		RawValue::SignedInt(v) => Some(v.to_string()),
		RawValue::UnsignedInt(v) => Some(v.to_string()),
		RawValue::Float(v) => Some(v.to_string()),
		RawValue::Boolean(v) => Some(if *v { "1".to_string() } else { "0".to_string() }),
		_ => None,
	}
}

fn symphonia_standard_tag_name(std_tag: &StandardTag) -> &'static str {

	match std_tag
	{
		StandardTag::TrackTitle(_) => "TrackTitle",
		StandardTag::Artist(_) => "Artist",
		StandardTag::Author(_) => "Author",
		StandardTag::AlbumArtist(_) => "AlbumArtist",
		StandardTag::Album(_) => "Album",
		StandardTag::TrackNumber(_) => "TrackNumber",
		StandardTag::TrackTotal(_) => "TrackTotal",
		StandardTag::Genre(_) => "Genre",
		_ => "Other",
	}
}

fn symphonia_probe_media_info(path: &str) -> Result<FFmpegProbeMediaInfo, String> {


	let file = File::open(path).map_err(|e| format!("open file failed: {:?}", e))?;
	let mss = MediaSourceStream::new(Box::new(file), Default::default());

	let mut hint = Hint::new();
	if let Some(ext) = Path::new(path)
		.extension()
		.and_then(|s| s.to_str())
	{
		hint.with_extension(ext);
	}

	let mut format = symphonia::default::get_probe()
		.probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
		.map_err(|e| format!("probe format failed: {:?}", e))?;

	let mut duration_ms: u64 = 0;
	if let Some(track) = format.default_track(TrackType::Audio)
	{
		if let (Some(tb), Some(dur)) = (track.time_base, track.duration)
		{
			if let Ok(ts) = Timestamp::try_from(dur.get())
			{
				let ms = tb.calc_time_saturating(ts).as_millis();
				duration_ms = ms.clamp(0, u64::MAX as i128) as u64;
			}
		}
		else if let (Some(tb), Some(n_frames)) = (track.time_base, track.num_frames)
		{
			if let Ok(ts) = Timestamp::try_from(n_frames)
			{
				let ms = tb.calc_time_saturating(ts).as_millis();
				duration_ms = ms.clamp(0, u64::MAX as i128) as u64;
			}
		}
	}

	let mut title: Option<String> = None;
	let mut artist: Option<String> = None;
	let mut album_artist: Option<String> = None;
	let mut album: Option<String> = None;
	let mut has_cover = false;

	let mut ingest = |std_tag: Option<&StandardTag>, value: String| {
		let v = value.trim();
		if v.is_empty()
		{
			return;
		}

		match std_tag
		{
			Some(StandardTag::TrackTitle(_)) =>
			{
				if title.is_none()
				{
					title = Some(v.to_string());
				}
			}
			Some(StandardTag::Artist(_)) | Some(StandardTag::Author(_)) =>
			{
				if artist.is_none()
				{
					artist = Some(v.to_string());
				}
			}
			Some(StandardTag::AlbumArtist(_)) =>
			{
				if album_artist.is_none()
				{
					album_artist = Some(v.to_string());
				}
			}
			Some(StandardTag::Album(_)) =>
			{
				if album.is_none()
				{
					album = Some(v.to_string());
				}
			}
			_ =>
			{}
		}
	};

	{
		let mut meta = format.metadata();
		if let Some(rev) = meta.skip_to_latest()
		{
			if !rev.media.visuals.is_empty()
			{
				has_cover = true;
			}
			for pt in rev.per_track.iter()
			{
				if !pt.metadata.visuals.is_empty()
				{
					has_cover = true;
				}
			}

			for t in rev.media.tags.iter()
			{
				if let Some(v) = symphonia_raw_value_to_string(&t.raw.value)
				{
					ingest(t.std.as_ref(), fix_mojibake_music_text(v));
				}
			}
			for pt in rev.per_track.iter()
			{
				for t in pt.metadata.tags.iter()
				{
					if let Some(v) = symphonia_raw_value_to_string(&t.raw.value)
					{
						ingest(t.std.as_ref(), fix_mojibake_music_text(v));
					}
				}
			}
		}
	}

	let codec_name = Path::new(path)
		.extension()
		.and_then(|s| s.to_str())
		.unwrap_or("")
		.to_ascii_lowercase();

	let mut tags: Vec<(String, String)> = Vec::new();
	if let Some(v) = title
	{
		tags.push(("title".to_string(), v));
	}
	if let Some(v) = artist
	{
		tags.push(("artist".to_string(), v));
	}
	if let Some(v) = album_artist
	{
		tags.push(("album_artist".to_string(), v));
	}
	if let Some(v) = album
	{
		tags.push(("album".to_string(), v));
	}

	Ok(FFmpegProbeMediaInfo { duration_ms, codec_name, tags, has_cover })
}

fn probe_media_info_prefer_symphonia(path: &str) -> Result<FFmpegProbeMediaInfo, String> {
	let ext = Path::new(path)
		.extension()
		.and_then(|s| s.to_str())
		.unwrap_or("")
		.to_ascii_lowercase();

	if ext == "mp3"
	{
		if let Ok(mut info) = symphonia_probe_media_info(path)
		{
			if info.duration_ms > 0
			{
				return Ok(info);
			}

			if let Ok(ff) = ffmpeg_probe_media_info(path)
			{
				if info.duration_ms == 0
				{
					info.duration_ms = ff.duration_ms;
				}
				if info.codec_name.is_empty()
				{
					info.codec_name = ff.codec_name;
				}
				if info.tags.is_empty()
				{
					info.tags = ff.tags;
				}
			}

			return Ok(info);
		}
	}

	ffmpeg_probe_media_info(path)
}

fn symphonia_collect_tags(path: &str) -> Result<Vec<TagItem>, String> {
	let file = File::open(path).map_err(|e| format!("无法打开文件: {:?}", e))?;
	let mss = MediaSourceStream::new(Box::new(file), Default::default());

	let mut hint = Hint::new();
	if let Some(ext) = Path::new(path)
		.extension()
		.and_then(|s| s.to_str())
	{
		hint.with_extension(ext);
	}

	let mut format = symphonia::default::get_probe()
		.probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
		.map_err(|e| format!("探测格式失败: {:?}", e))?;

	let mut out: Vec<TagItem> = Vec::new();

	{
		let mut meta = format.metadata();
		if let Some(rev) = meta.skip_to_latest()
		{
			for t in rev.media.tags.iter()
			{
				if let Some(v) = symphonia_raw_value_to_string(&t.raw.value)
				{
					out.push(TagItem { std_tag: t.std.clone(), key: t.raw.key.clone(), value: fix_mojibake_music_text(v) });
				}
			}
			for pt in rev.per_track.iter()
			{
				for t in pt.metadata.tags.iter()
				{
					if let Some(v) = symphonia_raw_value_to_string(&t.raw.value)
					{
						out.push(TagItem { std_tag: t.std.clone(), key: t.raw.key.clone(), value: fix_mojibake_music_text(v) });
					}
				}
			}
		}
	}

	if out.is_empty() { Err("未读取到可用的 metadata tags".to_string()) } else { Ok(out) }
}

fn tag_display_key(t: &TagItem) -> String {
	match t.std_tag.as_ref()
	{
		Some(k) => format!("{} ({})", symphonia_standard_tag_name(k), t.key),
		None => t.key.clone(),
	}
}

fn fill_now_playing_from_symphonia_tags(path: &str, tags: &[TagItem]) -> SongInfo {


	let mut np = SongInfo { path: path.to_string(), ..Default::default() };

	let mut genres: Vec<String> = Vec::new();
	let mut track_total: Option<u32> = None;

	for t in tags
	{
		let v = t.value.trim();
		if v.is_empty()
		{
			continue;
		}

		match t.std_tag.as_ref()
		{
			Some(StandardTag::TrackTitle(_)) =>
			{
				if np.title.is_empty()
				{
					np.title = v.to_string();
				}
			}
			Some(StandardTag::Artist(_)) | Some(StandardTag::Author(_)) =>
			{
				if np.author.is_empty()
				{
					np.author = v.to_string();
				}
			}
			Some(StandardTag::AlbumArtist(_)) =>
			{
				if np.album_artist.is_empty()
				{
					np.album_artist = v.to_string();
				}
			}
			Some(StandardTag::Album(_)) =>
			{
				if np.album.is_empty()
				{
					np.album = v.to_string();
				}
			}
			Some(StandardTag::TrackNumber(_)) =>
			{
				if np.track_number == 0
				{
					let (n, total) = parse_u32_pair_slash(v);
					if let Some(n) = n
					{
						np.track_number = n;
					}
					if total.is_some()
					{
						track_total = total;
					}
				}
			}
			Some(StandardTag::TrackTotal(_)) =>
			{
				if track_total.is_none()
				{
					track_total = parse_first_u32(v);
				}
			}
			Some(StandardTag::Genre(_)) => genres.extend(split_genres(v)),
			_ =>
			{}
		}
	}

	if np.title.is_empty() || np.author.is_empty()
	{
		let (a, t) = parse_author_title_from_path(path);
		if np.author.is_empty()
		{
			np.author = a;
		}
		if np.title.is_empty()
		{
			np.title = t;
		}
	}

	if np.album.is_empty()
	{
		if let Some(name) = album_name_from_parent_dir(path)
		{
			np.album = name;
		}
	}

	np.genres = {
		genres.sort();
		genres.dedup();
		genres
	};

	if let Some(total) = track_total
	{
		np.album_track_count = total;
	}

	np
}

fn fill_now_playing_from_ffmpeg_tags(path: &str, tags: &[(String, String)]) -> SongInfo {
	let mut np = SongInfo { path: path.to_string(), ..Default::default() };

	let mut genres: Vec<String> = Vec::new();

	for (k, v) in tags
	{
		let key = k.trim().to_ascii_lowercase();
		let v = v.trim();
		if v.is_empty()
		{
			continue;
		}

		match key.as_str()
		{
			"title" =>
			{
				if np.title.is_empty()
				{
					np.title = v.to_string();
				}
			}
			"artist" | "author" =>
			{
				if np.author.is_empty()
				{
					np.author = v.to_string();
				}
			}
			"album_artist" | "albumartist" =>
			{
				if np.album_artist.is_empty()
				{
					np.album_artist = v.to_string();
				}
			}
			"album" =>
			{
				if np.album.is_empty()
				{
					np.album = v.to_string();
				}
			}
			"track" | "tracknumber" =>
			{
				if np.track_number == 0
				{
					let (n, total) = parse_u32_pair_slash(v);
					if let Some(n) = n
					{
						np.track_number = n;
					}
					if let Some(total) = total
					{
						np.album_track_count = total;
					}
				}
			}
			"genre" => genres.extend(split_genres(v)),
			_ =>
			{}
		}
	}

	if np.title.is_empty() || np.author.is_empty()
	{
		let (a, t) = parse_author_title_from_path(path);
		if np.author.is_empty()
		{
			np.author = a;
		}
		if np.title.is_empty()
		{
			np.title = t;
		}
	}

	if np.album_artist.is_empty()
	{
		np.album_artist = np.author.clone();
	}

	if np.album.is_empty()
	{
		if let Some(name) = album_name_from_parent_dir(path)
		{
			np.album = name;
		}
	}

	np.genres = {
		genres.sort();
		genres.dedup();
		genres
	};

	np
}

unsafe fn collect_song_info(path: &str) -> SongInfo {
	let mut ff_duration_ms: u64 = 0;
	let mut ff_codec: String = String::new();
	let mut ff_tags: Vec<(String, String)> = Vec::new();
	let file_size = unsafe {
		get_file_flags(path)
			.map(|(_, sz)| sz)
			.unwrap_or(0)
	};

	if let Ok(probe) = ffmpeg_probe_media_info(path)
	{
		ff_duration_ms = probe.duration_ms;
		ff_codec = probe.codec_name;
		ff_tags = probe.tags;
	}

	let mut s = if !ff_tags.is_empty()
	{
		fill_now_playing_from_ffmpeg_tags(path, &ff_tags)
	}
	else
	{
		match symphonia_collect_tags(path)
		{
			Ok(tags) => fill_now_playing_from_symphonia_tags(path, &tags),
			Err(_) =>
			{
				let (author, title) = parse_author_title_from_path(path);
				SongInfo { path: path.to_string(), author, title, ..Default::default() }
			}
		}
	};

	if s.album_artist.is_empty() && !s.author.is_empty()
	{
		s.album_artist = s.author.clone();
	}

	if s.album.is_empty()
	{
		if let Some(name) = album_name_from_parent_dir(path)
		{
			s.album = name;
		}
	}

	if s.duration_ms == 0
	{
		s.duration_ms = ff_duration_ms;
		if s.duration_ms == 0
		{
			// Fallback: use decoder-provided duration if available.
			if let Ok(dec) = create_decoder(path)
			{
				let info = dec.info();
				if let Some(d) = info.duration_ms
				{
					s.duration_ms = d;
				}
				if s.codec.is_empty()
				{
					s.codec = info.codec_name.to_string();
				}
			}
		}
	}

	if s.codec.is_empty()
	{
		s.codec = ff_codec;
		if s.codec.is_empty()
		{
			s.codec = Path::new(path)
				.extension()
				.and_then(|s| s.to_str())
				.unwrap_or("")
				.to_ascii_lowercase();
		}
	}

	if s.duration_text.is_empty() && s.duration_ms > 0
	{
		s.duration_text = format_time(s.duration_ms);
	}

	if s.file_size == 0
	{
		s.file_size = file_size;
	}

	s
}

fn collect_song_info_scan_style(path: &str) -> SongInfo {
	let mut s = SongInfo { path: path.to_string(), ..Default::default() };

	s.file_size = unsafe {
		get_file_flags(path)
			.map(|(_, sz)| sz)
			.unwrap_or(0)
	};

	if let Ok(info) = probe_media_info_prefer_symphonia(path)
	{
		s.has_cover = info.has_cover;
		s.duration_ms = info.duration_ms;

		if s.duration_text.is_empty() && s.duration_ms > 0
		{
			s.duration_text = format_time(s.duration_ms);
		}

		s.codec = info.codec_name;
		if s.codec.is_empty()
		{
			s.codec = Path::new(path)
				.extension()
				.and_then(|s| s.to_str())
				.unwrap_or("")
				.to_ascii_lowercase();
		}

		// Same behavior as music library scan (music_db_upsert_song): keep the last value when tags repeat.
		for (k, v) in info.tags
		{
			let key = k.trim().to_ascii_lowercase();
			let v = v.trim();
			if v.is_empty()
			{
				continue;
			}

			match key.as_str()
			{
				"title" => s.title = v.to_string(),
				"artist" | "author" => s.author = v.to_string(),
				"album_artist" | "albumartist" =>
				{
					s.album_artist = v.to_string();
				}
				"album" => s.album = v.to_string(),
				_ =>
				{}
			}
		}
	};

	// Fallbacks are consistent with music library scan (title from filename; album from parent dir).
	if s.title.is_empty()
	{
		if let Some((base, _)) = Path::new(path)
			.file_name()
			.and_then(|s| s.to_str())
			.unwrap_or(path)
			.rsplit_once('.')
		{
			s.title = base.to_string();
		}
		else
		{
			s.title = Path::new(path)
				.file_name()
				.map(|v| v.to_string_lossy().to_string())
				.unwrap_or_else(|| path.to_string());
		}
	}

	if s.album.is_empty()
	{
		if let Some(name) = album_name_from_parent_dir(path)
		{
			s.album = name;
		}
	}

	if s.album_artist.is_empty() && !s.author.is_empty()
	{
		s.album_artist = s.author.clone();
	}

	s
}

fn collect_playlist_song_info(files: &[String]) -> Vec<SongInfo> {
	let mut out: Vec<SongInfo> = Vec::with_capacity(files.len());
	for p in files
	{
		let p = p.trim();
		if p.is_empty()
		{
			continue;
		}

		let path_fixed = if p.contains('/') { p.replace('/', "\\") } else { p.to_string() };
		out.push(collect_song_info_scan_style(&path_fixed));
	}
	out
}

// src\tags_ffmpeg.rs
unsafe fn ffmpeg_dict_collect(funcs: &AvFunctions, dict: *const AVDictionary) -> Vec<(String, String)> {
	let mut out: Vec<(String, String)> = Vec::new();
	if dict.is_null()
	{
		return out;
	}

	let mut prev: *const AVDictionaryEntry = null();
	loop
	{
		let e = (funcs.av_dict_get)(dict, b"\0".as_ptr() as *const i8, prev, AV_DICT_IGNORE_SUFFIX);
		if e.is_null()
		{
			break;
		}

		let key = if (*e).key.is_null()
		{
			"".to_string()
		}
		else
		{
			CStr::from_ptr((*e).key)
				.to_string_lossy()
				.into_owned()
		};
		let value = if (*e).value.is_null() { "".to_string() } else { decode_music_tag_bytes(CStr::from_ptr((*e).value).to_bytes()) };

		if !key.is_empty()
		{
			out.push((key, value));
		}
		prev = e as *const _;
	}

	out
}

fn ffmpeg_collect_metadata_tags(path: &str) -> Result<Vec<(String, String)>, String> {
	unsafe {
		let funcs = AV_FUNCS
			.as_ref()
			.ok_or("FFmpeg DLL 未加载")?;

		let mut ctx: *mut AVFormatContext = null_mut();
		let c_path = CString::new(path).map_err(|_| "无效路径")?;

		if (funcs.avformat_open_input)(&mut ctx, c_path.as_ptr(), null(), null_mut()) != 0
		{
			return Err("avformat_open_input 失败".to_string());
		}

		let _ = (funcs.avformat_find_stream_info)(ctx, null_mut());

		let mut tags: Vec<(String, String)> = Vec::new();

		tags.extend(ffmpeg_dict_collect(funcs, (*ctx).metadata as *const AVDictionary));

		let nb = (*ctx).nb_streams as isize;
		let streams = (*ctx).streams;
		if !streams.is_null()
		{
			for i in 0..nb
			{
				let st = *streams.offset(i);
				if st.is_null()
				{
					continue;
				}
				let codecpar = (*st).codecpar;
				if !codecpar.is_null() && (*codecpar).codec_type == AVMEDIA_TYPE_AUDIO
				{
					tags.extend(ffmpeg_dict_collect(funcs, (*st).metadata as *const AVDictionary));
					break;
				}
			}
		}

		(funcs.avformat_close_input)(&mut ctx);
		Ok(tags)
	}
}

// src\tags_text.rs
// ----------------------
// Text encoding fixes (Chinese tags)
// ----------------------

// Windows codepages
const CP_ACP: u32 = 0;
const CP_1252: u32 = 1252;
const CP_GBK: u32 = 936;
const CP_932: u32 = 932; // Shift-JIS (Japanese)
const CP_51932: u32 = 51932; // EUC-JP (Japanese)
const MB_ERR_INVALID_CHARS: u32 = 0x00000008;

fn contains_cjk(s: &str) -> bool {
	s.chars().any(
		|c| matches!(c as u32, 0x3000..=0x303F | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x2A6DF),
	)
}

fn is_han(c: char) -> bool {
	matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2A6DF)
}

fn is_kana(c: char) -> bool {
	matches!(c as u32, 0x3040..=0x30FF | 0x31F0..=0x31FF)
}

fn has_suspicious_control(s: &str) -> bool {
	s.chars().any(|c| {
		let u = c as u32;
		(u < 0x20 && !c.is_ascii_whitespace()) || matches!(u, 0x80..=0x9F)
	})
}

fn count_replacement_like(s: &str) -> usize {
	s.chars()
		.filter(|&c| c == '?' || c == '\u{FFFD}')
		.count()
}

fn is_mojibake_marker_ansi(c: char) -> bool {
	matches!(c, 'Ð' | 'Ò' | '¹' | 'º' | '»' | '¼' | '½' | '¾' | '¿' | 'µ' | '¸' | '±' | '£' | '¡' | '¤' | '¥')
}

fn music_text_score(s: &str) -> i32 {
	let mut score: i32 = 0;
	for c in s.chars()
	{
		let u = c as u32;
		if c == '?' || c == '\u{FFFD}'
		{
			score -= 40;
			continue;
		}
		if (u < 0x20 && !c.is_ascii_whitespace()) || matches!(u, 0x80..=0x9F)
		{
			score -= 60;
			continue;
		}
		if c.is_ascii_alphanumeric()
		{
			score += 3;
			continue;
		}
		if c.is_ascii_punctuation()
		{
			score += 1;
			continue;
		}
		if c.is_ascii_whitespace()
		{
			continue;
		}
		if is_mojibake_marker_ansi(c)
		{
			score -= 6;
			continue;
		}
		if is_han(c)
		{
			score += 8;
			continue;
		}
		if is_kana(c)
		{
			score += 7;
			continue;
		}
		if matches!(u, 0x00A0..=0x00FF)
		{
			score += 2;
			continue;
		}
		if matches!(u, 0x3000..=0x303F | 0xFF00..=0xFFEF)
		{
			score += 2;
			continue;
		}
	}
	score
}

fn music_text_rank(s: &str) -> (i32, i32) {
	let total = music_text_score(s);
	let len = s
		.chars()
		.filter(|c| !c.is_ascii_whitespace())
		.count()
		.max(1) as i32;
	let avg = (total * 1000) / len;
	(avg, total)
}

fn looks_like_mojibake_ansi(s: &str) -> bool {
	if s.is_empty() || contains_cjk(s)
	{
		return false;
	}

	let mut non_ascii = 0usize;
	let mut effective = 0usize;
	for ch in s.chars()
	{
		if (ch as u32) >= 0x80
		{
			non_ascii = non_ascii.saturating_add(1);
		}
		if ch.is_ascii_whitespace() || ch.is_ascii_digit() || ch.is_ascii_punctuation()
		{
			continue;
		}
		effective = effective.saturating_add(1);
	}

	if non_ascii < 2
	{
		return false;
	}

	let has_markers = s
		.chars()
		.any(|c| matches!(c, 'Ð' | 'Ò' | '¹' | 'º' | '»' | '¼' | '½' | '¾' | '¿' | 'µ' | '¸' | '±'));
	if has_markers
	{
		return true;
	}

	if effective == 0
	{
		return false;
	}

	// Heuristic: compare against "meaningful" characters (exclude digits/punct).
	non_ascii.saturating_mul(2) >= effective
}

unsafe fn decode_bytes_codepage(code_page: u32, flags: u32, bytes: &[u8]) -> Option<String> {
	if bytes.is_empty()
	{
		return Some(String::new());
	}

	let wide_len = MultiByteToWideChar(code_page, flags, bytes.as_ptr(), bytes.len() as i32, null_mut(), 0);
	if wide_len <= 0
	{
		return None;
	}

	let mut out: Vec<u16> = vec![0; wide_len as usize];
	let r = MultiByteToWideChar(code_page, flags, bytes.as_ptr(), bytes.len() as i32, out.as_mut_ptr(), wide_len);
	if r <= 0
	{
		return None;
	}

	Some(String::from_utf16_lossy(&out))
}

unsafe fn decode_bytes_codepage_fallback(code_page: u32, bytes: &[u8]) -> Option<String> {
	decode_bytes_codepage(code_page, MB_ERR_INVALID_CHARS, bytes).or_else(|| decode_bytes_codepage(code_page, 0, bytes))
}

fn decode_music_tag_bytes(bytes: &[u8]) -> String {
	if bytes.is_empty()
	{
		return String::new();
	}

	if let Ok(s) = std::str::from_utf8(bytes)
	{
		return fix_mojibake_music_text(s.to_string());
	}

	unsafe {
		let mut best: Option<((i32, i32), String)> = None;

		let mut consider = |s: Option<String>| {
			let Some(s) = s
			else
			{
				return;
			};
			let s = fix_mojibake_music_text(s);
			let rank = music_text_rank(&s);
			match &best
			{
				Some((best_rank, _)) if rank <= *best_rank =>
				{}
				_ => best = Some((rank, s)),
			}
		};

		consider(decode_bytes_codepage_fallback(CP_ACP, bytes));
		consider(decode_bytes_codepage_fallback(CP_GBK, bytes));
		consider(decode_bytes_codepage_fallback(CP_932, bytes));
		consider(decode_bytes_codepage_fallback(CP_51932, bytes));
		consider(decode_bytes_codepage_fallback(CP_1252, bytes));

		if let Some((_, s)) = best
		{
			return s;
		}
	}

	String::from_utf8_lossy(bytes).into_owned()
}

unsafe fn str_to_codepage_bytes(code_page: u32, s: &str) -> Option<Vec<u8>> {
	let wide: Vec<u16> = s.encode_utf16().collect();
	if wide.is_empty()
	{
		return Some(Vec::new());
	}

	let bytes_len = WideCharToMultiByte(code_page, 0, wide.as_ptr(), wide.len() as i32, null_mut(), 0, null(), null_mut());
	if bytes_len <= 0
	{
		return None;
	}
	let mut bytes: Vec<u8> = vec![0; bytes_len as usize];
	let w = WideCharToMultiByte(code_page, 0, wide.as_ptr(), wide.len() as i32, bytes.as_mut_ptr(), bytes_len, null(), null_mut());
	if w <= 0
	{
		return None;
	}
	Some(bytes)
}

fn str_to_latin1_bytes(s: &str) -> Option<Vec<u8>> {
	if s.chars().all(|c| (c as u32) <= 0xFF)
	{
		return Some(
			s.chars()
				.map(|c| (c as u32) as u8)
				.collect(),
		);
	}
	None
}

fn should_try_repair_text(s: &str) -> bool {
	if s.is_empty()
	{
		return false;
	}
	if has_suspicious_control(s)
	{
		return true;
	}
	if looks_like_mojibake_ansi(s)
	{
		return true;
	}
	count_replacement_like(s) >= 2
}

fn fix_mojibake_music_text(s: String) -> String {
	if !should_try_repair_text(&s)
	{
		return s;
	}

	let original_rank = music_text_rank(&s);
	let mut best = s.clone();
	let mut best_rank = original_rank;

	let mut consider = |cand: Option<String>| {
		let Some(cand) = cand
		else
		{
			return;
		};
		let rank = music_text_rank(&cand);
		if rank > best_rank
		{
			best_rank = rank;
			best = cand;
		}
	};

	unsafe {
		let has_cjk = contains_cjk(&s);
		let has_damage = has_suspicious_control(&s) || count_replacement_like(&s) > 0;

		// Fast path: if the original bytes (represented as Latin-1 chars) form valid UTF-8,
		// prefer UTF-8 decoding. This fixes broken ID3v2 text frames that are marked as
		// Latin-1 but actually store UTF-8 bytes (common in some Chinese MP3 releases).
		if let Some(b) = str_to_latin1_bytes(&s)
		{
			if let Ok(v) = std::str::from_utf8(&b)
			{
				let v = v.to_string();
				if !has_suspicious_control(&v) && count_replacement_like(&v) == 0
				{
					return v;
				}
			}
		}

		// 1) Common mojibake: bytes were CP936/CP932 but interpreted as Latin-1/CP1252.
		if let Some(b) = str_to_codepage_bytes(CP_1252, &s)
		{
			consider(
				std::str::from_utf8(&b)
					.ok()
					.map(|v| v.to_string()),
			);
			consider(decode_bytes_codepage_fallback(CP_GBK, &b));
			consider(decode_bytes_codepage_fallback(CP_932, &b));
			consider(decode_bytes_codepage_fallback(CP_51932, &b));
		}
		if let Some(b) = str_to_latin1_bytes(&s)
		{
			consider(
				std::str::from_utf8(&b)
					.ok()
					.map(|v| v.to_string()),
			);
			consider(decode_bytes_codepage_fallback(CP_GBK, &b));
			consider(decode_bytes_codepage_fallback(CP_932, &b));
			consider(decode_bytes_codepage_fallback(CP_51932, &b));
		}

		// 2) Transcode between GBK and Shift-JIS if the text already contains CJK but has obvious damage ('?' etc).
		if has_cjk
			&& has_damage
			&& let Some(b) = str_to_codepage_bytes(CP_GBK, &s)
		{
			consider(decode_bytes_codepage_fallback(CP_932, &b));
		}
		if has_cjk
			&& has_damage
			&& let Some(b) = str_to_codepage_bytes(CP_932, &s)
		{
			consider(decode_bytes_codepage_fallback(CP_GBK, &b));
		}
	}

	let best_total = best_rank.1;
	let original_total = original_rank.1;
	if best_total >= original_total + 10
	{
		return best;
	}
	s
}

// src\taskbar.rs
static mut g_TBL_PTR: i64 = 0; // ITaskbarList3 COM 对象指针

unsafe fn taskbar_init() {
	/// 初始化 ITaskbarList3 并提取 vtable 函数指针
	let tb: ITaskbarList3 = match CoCreateInstance(&TaskbarList, None, CLSCTX_ALL)
	{
		Ok(v) => v,
		Err(e) =>
		{
			eprintln!("[taskbar] CoCreateInstance(ITaskbarList3) failed: {:?}", e);
			return;
		}
	};

	if let Err(e) = tb.HrInit()
	{
		eprintln!("[taskbar] ITaskbarList3::HrInit failed: {:?}", e);
		return;
	}

	let ptr = transmute::<_, i64>(tb);
	let vtable_base = *(ptr as *const i64);

	g_TBL_PTR = ptr;
	taskbar_set_pos = transmute(*((vtable_base + 9 * 8 as i64) as *const i64));
	taskbar_set_state = transmute(*((vtable_base + 10 * 8 as i64) as *const i64));
}

unsafe fn taskbar_sync_player_state(state: PlayerState) {
	let flag = match state
	{
		PlayerState::Playing => 2,
		PlayerState::Paused => 8,
		PlayerState::Error => 4,
		PlayerState::Stopped | PlayerState::Idle => 0,
	};

	taskbar_set_state(g_TBL_PTR, UI_HWND, flag);
}

static mut taskbar_set_pos: extern "system" fn(_: i64, _: i64, _: u64, _: u64) -> i32 = __taskbar_set_pos;
extern "system" fn __taskbar_set_pos(_: i64, _: i64, _: u64, _: u64) -> i32 {
	0
}

static mut taskbar_set_state: extern "system" fn(_: i64, _: i64, _: i32) -> i32 = __taskbar_set_state;
extern "system" fn __taskbar_set_state(_: i64, _: i64, _: i32) -> i32 {
	0
}

// src\tool.rs
fn to_wstring(s: &str) -> Vec<u16> {
	s.encode_utf16()
		.chain(Some(0))
		.collect()
}

fn normalize_path_key(path: &str) -> String {
	path.replace('/', "\\")
		.to_ascii_lowercase()
}

#[inline(always)]
fn cmp_ascii_case_insensitive(a: &str, b: &str) -> std::cmp::Ordering {
	let ab = a.as_bytes();
	let bb = b.as_bytes();
	let n = ab.len().min(bb.len());
	for i in 0..n
	{
		let ca = ab[i].to_ascii_lowercase();
		let cb = bb[i].to_ascii_lowercase();
		if ca != cb
		{
			return ca.cmp(&cb);
		}
	}
	ab.len().cmp(&bb.len())
}

#[inline(always)]
fn starts_with_lowercase_prefix(full: &str, prefix_lower: &str) -> bool {
	let fb = full.as_bytes();
	let pb = prefix_lower.as_bytes();
	if pb.is_empty()
	{
		return true;
	}
	if fb.len() < pb.len()
	{
		return false;
	}
	for i in 0..pb.len()
	{
		if fb[i].to_ascii_lowercase() != pb[i]
		{
			return false;
		}
	}
	true
}

/// 格式化时间 (毫秒 -> MM:SS 或 HH:MM:SS)
fn format_time(ms: u64) -> String {
	let total_secs = ms / 1000;
	let hours = total_secs / 3600;
	let mins = (total_secs % 3600) / 60;
	let secs = total_secs % 60;

	if hours == 0
	{
		let mut s = String::with_capacity(5);
		unsafe {
			let out = s.as_mut_vec();
			out.push(b'0' + (mins / 10) as u8);
			out.push(b'0' + (mins % 10) as u8);
			out.push(b':');
			out.push(b'0' + (secs / 10) as u8);
			out.push(b'0' + (secs % 10) as u8);
		}
		return s;
	}

	let mut hour_digits: [u8; 20] = [0; 20];
	let mut pos = hour_digits.len();
	let mut h = hours;
	while h > 0
	{
		pos -= 1;
		hour_digits[pos] = b'0' + (h % 10) as u8;
		h /= 10;
	}

	let hour_len = hour_digits.len() - pos;
	let mut s = String::with_capacity(hour_len + 6);
	unsafe {
		let out = s.as_mut_vec();
		out.extend_from_slice(&hour_digits[pos..]);
		out.push(b':');
		out.push(b'0' + (mins / 10) as u8);
		out.push(b'0' + (mins % 10) as u8);
		out.push(b':');
		out.push(b'0' + (secs / 10) as u8);
		out.push(b'0' + (secs % 10) as u8);
	}
	s
}

/// 拆分路径为目录和文件名
fn split_path(path: &str) -> (&str, &str) {
	match path.rsplit_once('\\')
	{
		Some((d, n)) => (d, n),
		None => ("", path),
	}
}

/// 弹出 Windows 消息框
unsafe fn msg_box(text: &str, title: &str, u_type: u32) -> i32 {
	let t = to_wstring(text);
	let c = to_wstring(title);
	MessageBoxW(0, t.as_ptr(), c.as_ptr(), u_type)
}

unsafe fn is_dir(path: &str) -> bool {
	let attr = GetFileAttributesW(to_wstring(path).as_ptr());
	attr != 4294967295 && (attr & 16) != 0
}

unsafe fn is_file(path: &str) -> bool {
	let attr = GetFileAttributesW(to_wstring(path).as_ptr());
	attr != 4294967295 && (attr & 16) == 0
}

fn fix_scan_root(root: &str) -> String {
	let root = root.trim();
	if root.is_empty()
	{
		return String::new();
	}

	let mut s = root.replace('/', "\\");
	while s.ends_with('\\') && s.len() > 3
	{
		s.pop();
	}
	s
}

fn ui_join_root_rel(root: &str, rel: &str) -> String {
	let root = root.trim();
	let rel = rel.trim();
	if root.is_empty()
	{
		return rel.to_string();
	}
	if rel.is_empty()
	{
		return root.to_string();
	}
	if root.ends_with('\\') { format!("{}{}", root, rel) } else { format!("{}\\{}", root, rel) }
}

unsafe fn new_class(pt: *const u16, f: unsafe extern "system" fn(i64, u32, usize, i64) -> i64) {
	let wc = WNDCLASSW {
		style: 0x0002 | 0x0001, // CS_HREDRAW | CS_VREDRAW,
		lpfnWndProc: f,
		cbClsExtra: 0,
		cbWndExtra: 0,
		hInstance: 0,
		hIcon: 0,
		hCursor: 0,
		hbrBackground: 0,
		lpszMenuName: null_mut(),
		lpszClassName: pt,
	};

	RegisterClassW(&wc);
}

unsafe fn bin2vec(bin: &Vec<u8>) -> Vec<(String, String)> {
	let mut list = Vec::new();
	let mut pr = bin.as_ptr();
	let end = pr.add(bin.len());

	while pr < end
	{
		// 读取长度
		let k_len = u32::from_le(std::ptr::read_unaligned(pr as *const u32)) as usize;
		let v_len = u32::from_le(std::ptr::read_unaligned(pr.add(4) as *const u32)) as usize;

		// 读取字符串 (不进行 UTF-8 校验)
		let key = std::str::from_utf8_unchecked(from_raw_parts(pr.add(8), k_len)).to_owned();
		let val = std::str::from_utf8_unchecked(from_raw_parts(pr.add(8 + k_len), v_len)).to_owned();

		list.push((key, val));

		// 移动指针
		pr = pr.add(8 + k_len + v_len);
	}

	list
}

fn vec2bin(list: &[(String, String)]) -> Vec<u8> {
	let mut sv = Vec::with_capacity(
		list.len() * 8
			+ list
				.iter()
				.map(|(k, v)| k.len() + v.len())
				.sum::<usize>(),
	);

	for (k, v) in list
	{
		let k_len = k.len() as u32;
		let v_len = v.len() as u32;

		sv.extend_from_slice(&k_len.to_le_bytes());
		sv.extend_from_slice(&v_len.to_le_bytes());
		sv.extend_from_slice(k.as_bytes());
		sv.extend_from_slice(v.as_bytes());
	}

	sv
}

// src\track.rs
// 播放线程 - 负责 WASAPI 输出，极度轻量
// 这是消费者线程，只负责从 RingBuffer 读取数据并写入硬件

/// 宏：写入设备，失败则返回 true（表示需要退出并重建上下文）
/// 返回: true = 写入失败需退出, false = 成功
macro_rules! write_or_fail {
        ($render_client:expr, $frames:expr, $data:expr) => {{
                match $render_client.write_to_device($frames, $data, None)
                {
                        Ok(_) => false,
                        Err(e) =>
                        {
                                // AUDCLNT_E_DEVICE_INVALIDATED (0x88890004): 设备断开/失效
                                let is_invalidated = matches!(&e, wasapi::WasapiError::Windows(err) if err.code().0 as u32 == 0x88890004);
                                if is_invalidated {
                                        eprintln!("write_to_device 失败: {:?} (设备失效，准备重建)", e);
                                        set_last_retry_reason(RetryReason::DeviceInvalidated);
                                        g_device_change_tick.store(GetTickCount(), Ordering::SeqCst);
                                } else {
                                        eprintln!("write_to_device 失败: {:?}", e);
                                        set_last_retry_reason(RetryReason::RenderWriteFailed);
                                }
                                true
                        }
                }
        }};
}

/// 计算当前播放位置（毫秒）
#[inline(always)]
fn current_position_ms(output_sample_rate: usize, channels: usize) -> u64 {
	let samples = SAMPLES_PLAYED.load(Ordering::SeqCst);
	(samples * 1000) / (output_sample_rate as u64 * channels as u64)
}

/// 检查是否应该中止当前播放（切歌/播放列表变更等）
#[inline(always)]
fn should_abort_playback() -> bool {
	g_to_next.load(Ordering::SeqCst)
		|| g_to_prev.load(Ordering::SeqCst)
		|| is_pl_cmd.load(Ordering::SeqCst)
		|| g_pl_is_changed.load(Ordering::SeqCst)
}

/// 返回采样类型的字符串表示
#[inline(always)]
const fn sample_type_str(is_float: bool) -> &'static str {
	if is_float { "float" } else { "int" }
}

unsafe fn stop_decode_task_and_wait() {
	g_dec_stop.store(true, Ordering::SeqCst);
	// 唤醒可能在 EOF/暂停 等待中的解码线程
	SetEvent(g_ev_dec_wakeup);
	// 唤醒可能在等待 RingBuffer 空间的解码线程
	SetEvent(g_ev_ring_space);
	WaitForSingleObject(g_ev_dec_idle, 0xFFFFFFFF);
}

/// 播放线程入口（精简版）
/// 返回: None = 正常结束或切歌, Some(ms) = 需要从 ms 位置重放
unsafe fn play_track(
	song: &SongInfo, start_ms: u64, decode_tx: &mpsc::Sender<DecodeCommand>, consumer: &mut ringbuf::HeapCons<f64>,
) -> Option<u64> {
	let path = song.path.as_str();
	// 如果独占模式只能降位（例如源 24bit 但设备仅支持 16bit），是否仍坚持独占
	const EXCLUSIVE_ALLOW_DOWNBIT: bool = false;

	// Shared 模式优先使用设备 MixFormat：便于让内置 rubato 接管重采样，避免依赖系统 SRC（对蓝牙/LDAC 更可控）
	const SHARED_PREFER_MIX_FORMAT: bool = true;

	// 等待恢复运行，但暂停期间仍允许控制命令中断。
	loop
	{
		if should_abort_playback()
		{
			return None;
		}

		// Put command-wakeup first so "next/prev while paused" won't be eaten by resume.
		let hs = [g_ev_pl_quit, g_ev_resume];
		let r = WaitForMultipleObjects(2, hs.as_ptr(), 0, 0xFFFFFFFF);
		if r == 0
		{
			continue;
		}
		if r == 1
		{
			break;
		}
		return None;
	}

	// === 第一步：使用插件化解码器获取音频参数 ===
	let decoder_info = match create_decoder(path)
	{
		Ok(dec) =>
		{
			let info = dec.info();
			if info.bits_per_sample == 0
			{
				eprintln!("[pl] 播放 {} [{}] ({}Hz/?bit/{}ch)", path, info.codec_name, info.sample_rate, info.channels);
			}
			else
			{
				eprintln!(
					"[pl] 播放 {} [{}] ({}Hz/{}bit/{}ch)",
					path, info.codec_name, info.sample_rate, info.bits_per_sample, info.channels
				);
			}
			info
		}
		Err(e) =>
		{
			eprintln!("[pl] 无法打开文件: {}", e);
			SAMPLES_PLAYED.store(0, Ordering::SeqCst);
			TRACK_DURATION_MS.store(0, Ordering::SeqCst);
			set_player_state(PlayerState::Stopped);
			set_last_retry_reason(RetryReason::TrackOpenFailed);
			return None;
		}
	};

	let sample_rate = decoder_info.sample_rate;
	let channels = decoder_info.channels;
	let src_bits = decoder_info.bits_per_sample as usize;
	let src_bits_opt = if src_bits == 0 { None } else { Some(src_bits) };
	let bits_per_sample = src_bits_opt.unwrap_or(16);

	// === 第二步：初始化 WASAPI ===
	let enumerator = match DeviceEnumerator::new()
	{
		Ok(e) => e,
		Err(e) =>
		{
			eprintln!("[pl] DeviceEnumerator 创建失败: {:?}", e);
			set_last_retry_reason(RetryReason::WasapiInitFailed);
			return Some(start_ms);
		}
	};

	let device = match enumerator.get_default_device(&Direction::Render)
	{
		Ok(d) => d,
		Err(e) =>
		{
			eprintln!("[pl] 获取默认输出设备失败: {:?}", e);
			set_last_retry_reason(RetryReason::WasapiInitFailed);
			return Some(start_ms);
		}
	};

	let wave_format = WaveFormat::new(bits_per_sample, bits_per_sample, &SampleType::Int, sample_rate as usize, channels as usize, None);

	let mut buffer_duration_hns = unsafe { g_buffer_duration_hns };
	if buffer_duration_hns <= 0
	{
		buffer_duration_hns = 200_000i64;
		unsafe {
			g_buffer_duration_hns = buffer_duration_hns;
		}
	}
	let prefer_exclusive = g_def_exclusive;

	// 标准采样率列表（独占优先尝试更高采样率，避免在设备支持 96k 时先命中 48k）
	let standard_rates: &[usize] = &[96000, 48000, 44100, 192000];

	let mut needs_resample = false;
	let mut output_sample_rate = sample_rate as usize;

	// 预取 mix format，作为独占模式的兜底格式
	let mix_format = device
		.get_iaudioclient()
		.ok()
		.and_then(|c| c.get_mixformat().ok());

	// 记录最终是否实际进入独占
	let mut is_exclusive_mode = false;

	if let Some(ref mix_fmt) = mix_format
	{
		let mix_bits = mix_fmt.get_bitspersample();
		let mix_valid = mix_fmt.get_validbitspersample();
		let mix_rate = mix_fmt.get_samplespersec();
		let mix_is_float = mix_fmt.get_subformat().ok() == Some(SampleType::Float);
		eprintln!(
			"[pl] 设备 MixFormat: {}Hz {}bit/{}bits {} (mask={:#010x})",
			mix_rate,
			mix_bits,
			mix_valid,
			sample_type_str(mix_is_float),
			mix_fmt.get_dwchannelmask()
		);
	}
	else
	{
		eprintln!("[pl] 设备 MixFormat: 获取失败");
	}

	let (mut audio_client, stream_mode, format_used) = {
		let mut stream_mode;
		let mut format_used;
		let mut audio_client;

		if prefer_exclusive
		{
			// 优先尝试：源采样率 -> 常见采样率 -> 设备 mix 采样率（兜底）
			let mut rate_candidates: Vec<usize> = vec![sample_rate as usize];
			for &r in standard_rates
			{
				if !rate_candidates.contains(&r)
				{
					rate_candidates.push(r);
				}
			}
			if let Some(ref mix_fmt) = mix_format
			{
				let mix_rate = mix_fmt.get_samplespersec() as usize;
				if !rate_candidates.contains(&mix_rate)
				{
					rate_candidates.push(mix_rate);
				}
			}

			// 允许尝试不同存储位宽/子格式，避免因 24bit PCM 不被支持直接回退
			let mut fmt_candidates: Vec<(usize, usize, SampleType)> = Vec::new();
			let mut push_unique = |item: (usize, usize, SampleType), list: &mut Vec<(usize, usize, SampleType)>| {
				if !list.contains(&item)
				{
					list.push(item);
				}
			};

			// bits_per_sample 过低（例如 DSF/DSD 常见的 8bit）通常不代表真实 PCM 精度；
			// 这种情况下不要只生成 8bit/32-8bit 等候选，否则可能错过设备仅支持的 24bit/32f 输出格式。
			let src_bits_for_candidates = src_bits_opt.filter(|&b| b >= 16);

			if let Some(src_bits) = src_bits_for_candidates
			{
				if src_bits < 32
				{
					// 对于 24bit 等非 16/32 整数位深，优先尝试 32-bit 容器（24-in-32）
					// 许多驱动对 packed-24(3 bytes) 支持不稳定/易出噪音，但对 24-in-32 更稳。
					if src_bits != 16
					{
						push_unique((32, src_bits, SampleType::Int), &mut fmt_candidates); // 24bit 源 -> 32bit 容器
					}
				}
				push_unique((src_bits, src_bits, SampleType::Int), &mut fmt_candidates);
				if src_bits < 32
				{
					push_unique((32, src_bits, SampleType::Int), &mut fmt_candidates); // 兜底：保留 32 容器候选
				}
				if src_bits <= 16
				{
					// 某些设备/驱动在 Exclusive 下不接受 16bit，但接受 24bit（常见于“固定 24bit/96k”链路）。
					// 对 16bit 源来说，升位到 24bit 只会增加零/更细的量化网格，不会丢失信息。
					push_unique((32, 24, SampleType::Int), &mut fmt_candidates); // 24-in-32
					push_unique((24, 24, SampleType::Int), &mut fmt_candidates); // packed-24
				}
				push_unique((32, 32, SampleType::Int), &mut fmt_candidates); // 32bit 整型输出（部分驱动只接受 32/32 int）
				push_unique((32, 32, SampleType::Float), &mut fmt_candidates);
				if src_bits > 16
				{
					push_unique((16, 16, SampleType::Int), &mut fmt_candidates); // 降位 16bit 兜底，避免完全回退共享
				}
			}
			else
			{
				// 源位深未知或过低：避免把 16bit 当成“确定源位深”并优先命中，优先尝试更高精度
				push_unique((32, 32, SampleType::Float), &mut fmt_candidates);
				push_unique((32, 32, SampleType::Int), &mut fmt_candidates);
				push_unique((32, 24, SampleType::Int), &mut fmt_candidates);
				push_unique((24, 24, SampleType::Int), &mut fmt_candidates);
				push_unique((16, 16, SampleType::Int), &mut fmt_candidates);
			}
			if let Some(ref mix_fmt) = mix_format
			{
				if let Ok(sub) = mix_fmt.get_subformat()
				{
					let variant = (mix_fmt.get_bitspersample() as usize, mix_fmt.get_validbitspersample() as usize, sub);
					if !fmt_candidates.contains(&variant)
					{
						fmt_candidates.push(variant);
					}
				}
			}

			eprintln!(
				"[pl] Exclusive 候选采样率: {:?}; 候选格式: {:?}",
				rate_candidates,
				fmt_candidates
					.iter()
					.map(|(s, v, ty)| format!("{}bit/{}bits {}", s, v, sample_type_str(*ty == SampleType::Float)))
					.collect::<Vec<_>>()
			);

			let mut chosen: Option<(wasapi::AudioClient, WaveFormat)> = None;

			'outer: for &rate in &rate_candidates
			{
				for &(store_bits, valid_bits, ref sample_ty) in &fmt_candidates
				{
					let candidate = WaveFormat::new(store_bits, valid_bits, sample_ty, rate, channels as usize, None);
					let mut client = match device.get_iaudioclient()
					{
						Ok(c) => c,
						Err(_) => continue,
					};

					match client.is_supported_exclusive_with_quirks(&candidate)
					{
						Ok(fmt) =>
						{
							let fmt_store = fmt.get_bitspersample();
							let fmt_valid_raw = fmt.get_validbitspersample();
							let fmt_valid = if fmt_valid_raw == 0 { fmt_store } else { fmt_valid_raw };
							eprintln!(
								"[pl] Exclusive 支持: {}Hz {}bit/{}bits {}",
								fmt.get_samplespersec(),
								fmt_store,
								fmt_valid,
								sample_type_str(fmt.get_subformat().ok() == Some(SampleType::Float))
							);
							chosen = Some((client, fmt));
							break 'outer;
						}
						Err(e) =>
						{
							eprintln!(
								"[pl] Exclusive 不支持: {}Hz {}bit/{}bits {} -> {:?}",
								rate,
								store_bits,
								valid_bits,
								sample_type_str(*sample_ty == SampleType::Float),
								e
							);
						}
					}
				}
			}

			if let Some((client, fmt)) = chosen
			{
				// 如果独占格式会降低位深且不允许降位，则直接走 Shared（可保留 32f 路径）
				let mut fmt_bits = fmt.get_validbitspersample() as usize;
				if fmt_bits == 0
				{
					// 某些驱动返回 WAVEFORMATEX（非 extensible）时 validbits=0；此时应视为等于容器位宽
					fmt_bits = fmt.get_bitspersample() as usize;
				}
				let fmt_is_float = fmt.get_subformat().ok() == Some(SampleType::Float);

				let mut fallback_to_shared = false;
				if !EXCLUSIVE_ALLOW_DOWNBIT
				{
					if let Some(src_bits) = src_bits_opt
					{
						if src_bits > fmt_bits
						{
							fallback_to_shared = true;
							eprintln!("[pl] 独占仅支持 {}bit, 源 {}bit，切换 Shared 以避免降位", fmt_bits, src_bits);
						}
					}
					else if !fmt_is_float && fmt_bits <= 16
					{
						// 源位深未知时，不要把 “16bit 独占输出” 当成“肯定没有降位”
						// 若 Shared 的 MixFormat 更高精度（常见 32f），则优先切到 Shared 避免潜在降位
						if let Some(ref mix_fmt) = mix_format
						{
							let mut mix_valid = mix_fmt.get_validbitspersample() as usize;
							if mix_valid == 0
							{
								mix_valid = mix_fmt.get_bitspersample() as usize;
							}
							let mix_is_float = mix_fmt.get_subformat().ok() == Some(SampleType::Float);
							if mix_is_float || mix_valid > fmt_bits
							{
								fallback_to_shared = true;
								eprintln!(
									"[pl] 独占仅支持 {}bit 且源位深未知，切换 Shared 以避免潜在降位 (MixFormat: {}Hz {}bit/{}bits {})",
									fmt_bits,
									mix_fmt.get_samplespersec(),
									mix_fmt.get_bitspersample(),
									mix_fmt.get_validbitspersample(),
									sample_type_str(mix_is_float)
								);
							}
						}
					}
				}

				if fallback_to_shared
				{
					audio_client = match device.get_iaudioclient()
					{
						Ok(c) => c,
						Err(_) => return None,
					};
					stream_mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns };

					if SHARED_PREFER_MIX_FORMAT && let Some(ref mix_fmt) = mix_format
					{
						output_sample_rate = mix_fmt.get_samplespersec() as usize;
						needs_resample = output_sample_rate != sample_rate as usize;

						let mix_bits = mix_fmt.get_bitspersample();
						let mix_valid = mix_fmt.get_validbitspersample();
						let mix_is_float = mix_fmt.get_subformat().ok() == Some(SampleType::Float);
						let mix_ch = mix_fmt.get_nchannels();
						eprintln!(
							"[pl] Shared 使用 MixFormat: {}Hz {}bit/{}bits {} ({}ch)",
							output_sample_rate,
							mix_bits,
							mix_valid,
							sample_type_str(mix_is_float),
							mix_ch
						);

						format_used = mix_fmt.clone();
					}
					else
					{
						// 回退到 Shared 模式：重置重采样标志（autoconvert 会处理）
						needs_resample = false;
						output_sample_rate = sample_rate as usize;
						format_used = wave_format.clone();
					}
				}
				else
				{
					output_sample_rate = fmt.get_samplespersec() as usize;
					needs_resample = output_sample_rate != sample_rate as usize;

					let out_bits = fmt.get_bitspersample();
					if needs_resample
					{
						eprintln!(
							"[pl] 使用 WASAPI Exclusive 模式 (重采样: {}Hz -> {}Hz, {}bit {})",
							sample_rate,
							output_sample_rate,
							out_bits,
							sample_type_str(fmt_is_float)
						);
					}
					else
					{
						eprintln!(
							"[pl] 使用 WASAPI Exclusive 模式 ({}Hz, {}bit {})",
							output_sample_rate,
							out_bits,
							sample_type_str(fmt_is_float)
						);
					}

					audio_client = client;
					stream_mode = StreamMode::EventsExclusive { period_hns: buffer_duration_hns };
					format_used = fmt;
				}
			}
			else
			{
				eprintln!("[pl] Exclusive 所有格式都不支持，回退到 Shared");
				audio_client = match device.get_iaudioclient()
				{
					Ok(c) => c,
					Err(_) => return None,
				};
				stream_mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns };
				if SHARED_PREFER_MIX_FORMAT && let Some(ref mix_fmt) = mix_format
				{
					output_sample_rate = mix_fmt.get_samplespersec() as usize;
					needs_resample = output_sample_rate != sample_rate as usize;

					let mix_bits = mix_fmt.get_bitspersample();
					let mix_valid = mix_fmt.get_validbitspersample();
					let mix_is_float = mix_fmt.get_subformat().ok() == Some(SampleType::Float);
					let mix_ch = mix_fmt.get_nchannels();
					eprintln!(
						"[pl] Shared 使用 MixFormat: {}Hz {}bit/{}bits {} ({}ch)",
						output_sample_rate,
						mix_bits,
						mix_valid,
						sample_type_str(mix_is_float),
						mix_ch
					);

					format_used = mix_fmt.clone();
				}
				else
				{
					// 回退到 Shared 模式：重置重采样标志（autoconvert 会处理）
					needs_resample = false;
					output_sample_rate = sample_rate as usize;
					format_used = wave_format.clone();
				}
			}
		}
		else
		{
			eprintln!("[pl] 使用 WASAPI Shared 模式");
			audio_client = match device.get_iaudioclient()
			{
				Ok(c) => c,
				Err(_) => return None,
			};
			if SHARED_PREFER_MIX_FORMAT && let Some(ref mix_fmt) = mix_format
			{
				output_sample_rate = mix_fmt.get_samplespersec() as usize;
				needs_resample = output_sample_rate != sample_rate as usize;

				let mix_bits = mix_fmt.get_bitspersample();
				let mix_valid = mix_fmt.get_validbitspersample();
				let mix_is_float = mix_fmt.get_subformat().ok() == Some(SampleType::Float);
				let mix_ch = mix_fmt.get_nchannels();
				eprintln!(
					"[pl] Shared 使用 MixFormat: {}Hz {}bit/{}bits {} ({}ch)",
					output_sample_rate,
					mix_bits,
					mix_valid,
					sample_type_str(mix_is_float),
					mix_ch
				);

				stream_mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns };
				format_used = mix_fmt.clone();
			}
			else
			{
				stream_mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns };
				format_used = wave_format.clone();
			}
		}

		(audio_client, stream_mode, format_used)
	};

	// 初始化 WASAPI Client：独占失败时优先自动回退到 Shared（避免直接暂停）
	#[inline]
	fn align_period_hns(sample_rate_hz: usize, desired_hns: i64, align_frames: usize) -> i64 {
		if sample_rate_hz == 0 || desired_hns <= 0 || align_frames == 0
		{
			return desired_hns;
		}

		let sr = sample_rate_hz as u64;
		let desired_hns_u = desired_hns as u64;
		let desired_frames = (sr
			.saturating_mul(desired_hns_u)
			.saturating_add(10_000_000 - 1))
			/ 10_000_000;
		let align = align_frames as u64;
		let aligned_frames = ((desired_frames + align - 1) / align) * align;
		let aligned_hns = (aligned_frames
			.saturating_mul(10_000_000)
			.saturating_add(sr - 1))
			/ sr;
		(aligned_hns as i64).max(1)
	}

	let mut stream_mode = stream_mode;
	let mut format_used = format_used;
	let mut tried_shared_fallback = false;
	let mut retried_once = false;
	let mut tried_getbuffersize_align = false;
	let mut align_retry_idx: usize = 0;
	let mut align_period_candidates: [i64; 3] = [0, 0, 0];

	// 预先计算独占 period：对齐到设备最小 period + 块大小（典型 HDA 128 bytes）
	// 避免每次都触发 0x88890019 再“握手”一次。
	if matches!(stream_mode, StreamMode::EventsExclusive { .. })
	{
		if let Ok(p) = audio_client.calculate_aligned_period_near(buffer_duration_hns, Some(128), &format_used)
		{
			buffer_duration_hns = p;
			unsafe {
				g_buffer_duration_hns = buffer_duration_hns;
			}
			stream_mode = StreamMode::EventsExclusive { period_hns: p };
		}
	}

	'init: loop
	{
		match audio_client.initialize_client(&format_used, &Direction::Render, &stream_mode)
		{
			Ok(()) => break,
			Err(e) =>
			{
				let is_exclusive = matches!(stream_mode, StreamMode::EventsExclusive { .. });
				let err_code = match &e
				{
					wasapi::WasapiError::Windows(err) => err.code().0 as u32,
					_ => 0,
				};

				// AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED (0x88890019): period/buffer 对应的 frame 数不符合设备要求
				if is_exclusive && err_code == 0x88890019
				{
					// 先尝试按 Windows 建议：用 GetBufferSize 返回的对齐 frame 数反推 period
					if !tried_getbuffersize_align
					{
						tried_getbuffersize_align = true;
						let sr = format_used.get_samplespersec() as u64;
						if sr > 0
						{
							if let Ok(frames) = audio_client.get_bufferframecount()
							{
								let p = ((frames as u64) * 10_000_000u64).saturating_add(sr - 1) / sr;
								let p = (p as i64).max(1);
								let cur = match stream_mode
								{
									StreamMode::EventsExclusive { period_hns } => period_hns,
									_ => 0,
								};
								if p != cur
								{
									eprintln!("[pl] 独占 initialize_client 失败(0x88890019)，按 GetBufferSize 对齐 period_hns={} 重试", p);
									buffer_duration_hns = p;
									unsafe {
										g_buffer_duration_hns = buffer_duration_hns;
									}
									drop(audio_client);
									audio_client = match device.get_iaudioclient()
									{
										Ok(c) => c,
										Err(e) =>
										{
											ResetEvent(g_ev_resume);
											PostMessageW(G_HWND, WM_DEVICE_IN_USE, 0, 0);
											eprintln!("get_iaudioclient 失败: {:?}", e);
											set_last_retry_reason(RetryReason::DeviceInUse);
											return Some(start_ms);
										}
									};
									stream_mode = StreamMode::EventsExclusive { period_hns: p };
									continue 'init;
								}
							}
						}
					}

					if align_period_candidates[0] == 0
					{
						let sr = format_used.get_samplespersec() as usize;
						let base = match stream_mode
						{
							StreamMode::EventsExclusive { period_hns } => period_hns,
							_ => buffer_duration_hns,
						};
						let p64 = align_period_hns(sr, base, 64);
						let p128 = align_period_hns(sr, base, 128);
						let p256 = align_period_hns(sr, base, 256);
						align_period_candidates = [p64, p128, p256];
					}

					while align_retry_idx < align_period_candidates.len()
					{
						let p = align_period_candidates[align_retry_idx];
						align_retry_idx += 1;
						if p <= 0
						{
							continue;
						}
						eprintln!("[pl] 独占 initialize_client 失败(0x88890019)，尝试 period_hns={} 重试", p);
						drop(audio_client);
						audio_client = match device.get_iaudioclient()
						{
							Ok(c) => c,
							Err(e) =>
							{
								ResetEvent(g_ev_resume);
								PostMessageW(G_HWND, WM_DEVICE_IN_USE, 0, 0);
								eprintln!("get_iaudioclient 失败: {:?}", e);
								set_last_retry_reason(RetryReason::DeviceInUse);
								return Some(start_ms);
							}
						};
						stream_mode = StreamMode::EventsExclusive { period_hns: p };
						buffer_duration_hns = p;
						unsafe {
							g_buffer_duration_hns = buffer_duration_hns;
						}
						continue 'init;
					}
				}

				// 独占初始化失败：回退到 Shared，避免直接暂停（Shared 仍可能失败：例如被其他应用独占占用）
				if is_exclusive && !tried_shared_fallback
				{
					tried_shared_fallback = true;
					eprintln!("[pl] 独占 initialize_client 失败，回退 Shared: {:?}", e);

					drop(audio_client);
					audio_client = match device.get_iaudioclient()
					{
						Ok(c) => c,
						Err(e) =>
						{
							ResetEvent(g_ev_resume);
							PostMessageW(G_HWND, WM_DEVICE_IN_USE, 0, 0);
							eprintln!("get_iaudioclient 失败: {:?}", e);
							set_last_retry_reason(RetryReason::DeviceInUse);
							return Some(start_ms);
						}
					};

					stream_mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns };
					if SHARED_PREFER_MIX_FORMAT && let Some(ref mix_fmt) = mix_format
					{
						output_sample_rate = mix_fmt.get_samplespersec() as usize;
						needs_resample = output_sample_rate != sample_rate as usize;

						let mix_bits = mix_fmt.get_bitspersample();
						let mix_valid = mix_fmt.get_validbitspersample();
						let mix_is_float = mix_fmt.get_subformat().ok() == Some(SampleType::Float);
						let mix_ch = mix_fmt.get_nchannels();
						eprintln!(
							"[pl] Shared 使用 MixFormat: {}Hz {}bit/{}bits {} ({}ch)",
							output_sample_rate,
							mix_bits,
							mix_valid,
							sample_type_str(mix_is_float),
							mix_ch
						);

						format_used = mix_fmt.clone();
					}
					else
					{
						needs_resample = false;
						output_sample_rate = sample_rate as usize;
						format_used = wave_format.clone();
					}
					continue;
				}

				// 非上述情况：做一次通用重试（处理短暂状态问题）
				if !retried_once
				{
					retried_once = true;
					eprintln!("[pl] initialize_client 失败: {:?}，100ms 后重试一次", e);
					Sleep(100);

					drop(audio_client);
					audio_client = match device.get_iaudioclient()
					{
						Ok(c) => c,
						Err(e) =>
						{
							ResetEvent(g_ev_resume);
							PostMessageW(G_HWND, WM_DEVICE_IN_USE, 0, 0);
							eprintln!("get_iaudioclient 失败: {:?}", e);
							set_last_retry_reason(RetryReason::DeviceInUse);
							return Some(start_ms);
						}
					};
					continue;
				}

				eprintln!("[pl] initialize_client 最终失败: {:?}", e);
				ResetEvent(g_ev_resume);
				PostMessageW(G_HWND, WM_DEVICE_IN_USE, 0, 0);
				set_last_retry_reason(RetryReason::DeviceInUse);
				return Some(start_ms);
			}
		}
	}

	// 初始化成功后同步当前 period（用于下一次初始化的 desired 值）
	if let StreamMode::EventsExclusive { period_hns } = stream_mode
	{
		unsafe {
			g_buffer_duration_hns = period_hns;
		}
	}

	g_is_exclusive.store(matches!(stream_mode, StreamMode::EventsExclusive { .. }), Ordering::SeqCst);

	let in_channels = channels as usize;
	let out_channels = format_used.get_nchannels() as usize;
	let block_align = format_used.get_blockalign() as usize;
	let out_store_bits = format_used.get_bitspersample() as usize;
	let mut out_valid_bits = format_used.get_validbitspersample() as usize;
	if out_valid_bits == 0
	{
		out_valid_bits = out_store_bits;
	}
	let out_is_float = format_used.get_subformat().ok() == Some(wasapi::SampleType::Float);

	eprintln!(
		"[pl] 输出: {}bit/{}bits {} ({}ch, block_align={})",
		out_store_bits,
		out_valid_bits,
		sample_type_str(out_is_float),
		out_channels,
		block_align
	);
	if in_channels != out_channels
	{
		eprintln!("[pl] 声道转换: {}ch -> {}ch", in_channels, out_channels);
	}

	let render_client = match audio_client.get_audiorenderclient()
	{
		Ok(r) => r,
		Err(e) =>
		{
			eprintln!("get_audiorenderclient 失败: {:?}", e);
			set_last_retry_reason(RetryReason::WasapiInitFailed);
			return Some(start_ms);
		}
	};

	let event = match audio_client.set_get_eventhandle()
	{
		Ok(e) => e,
		Err(e) =>
		{
			eprintln!("set_get_eventhandle 失败: {:?}", e);
			set_last_retry_reason(RetryReason::WasapiInitFailed);
			return Some(start_ms);
		}
	};
	let buffer_size = audio_client
		.get_bufferframecount()
		.unwrap_or(4096) as usize;
	let is_exclusive = matches!(stream_mode, StreamMode::EventsExclusive { .. });

	// === 第三步：启动解码任务（常驻解码线程）===
	// 等待解码线程空闲，确保 RingBuffer clear 不与 producer 并发
	WaitForSingleObject(g_ev_dec_idle, 0xFFFFFFFF);
	consumer.clear();

	// 重置控制标志（任务级）
	g_dec_stop.store(false, Ordering::SeqCst);
	g_seek_req_ms.store(-1, Ordering::SeqCst);
	g_seek_to_ms.store(-1, Ordering::SeqCst);
	g_seek_just.store(false, Ordering::SeqCst);
	SAMPLES_PLAYED.store((start_ms * output_sample_rate as u64) / 1000 * channels as u64, Ordering::SeqCst);

	// 先标记为忙，避免 start_stream 失败等路径立即 wait() 误判
	ResetEvent(g_ev_dec_idle);
	if decode_tx
		.send(DecodeCommand::Start { path: path.to_string(), start_ms, output_sample_rate, channels })
		.is_err()
	{
		// 解码线程已退出：恢复空闲事件，避免后续 wait 永久阻塞
		SetEvent(g_ev_dec_idle);
		eprintln!("[decode] 发送解码任务失败");
		set_player_state(PlayerState::Stopped);
		set_last_retry_reason(RetryReason::TrackOpenFailed);
		return None;
	}

	let ring_capacity = consumer.capacity();

	// 等待一小段时间让解码线程预填充缓冲
	Sleep(50);

	if let Err(e) = audio_client.start_stream()
	{
		eprintln!("start_stream 失败: {:?}", e);
		if let wasapi::WasapiError::Windows(ref err) = e
		{
			let code = err.code().0 as u32;
			if code == 0x88890004
			{
				set_last_retry_reason(RetryReason::DeviceInvalidated);
				g_device_change_tick.store(GetTickCount(), Ordering::SeqCst);
			}
			else
			{
				set_last_retry_reason(RetryReason::StartStreamFailed);
			}
		}
		else
		{
			set_last_retry_reason(RetryReason::StartStreamFailed);
		}
		stop_decode_task_and_wait();
		return Some(start_ms);
	}

	smtc_set_now_playing_from_song(song);
	set_player_state(PlayerState::Playing);

	// 存储输出参数用于进度计算
	OUTPUT_SAMPLE_RATE.store(output_sample_rate, Ordering::SeqCst);
	OUTPUT_CHANNELS.store(channels as usize, Ordering::SeqCst);
	let duration_ms = decoder_info
		.duration_ms
		.unwrap_or(song.duration_ms);
	TRACK_DURATION_MS.store(duration_ms, Ordering::SeqCst);

	// 上次发送进度通知的时间（毫秒）
	PostMessageW(G_HWND, WM_PROGRESS, start_ms as usize, duration_ms.min(i64::MAX as u64) as i64);
	let mut last_progress_ms: u64 = start_ms;

	// 简单的 RNG 状态用于 TPDF Dither
	let mut rng_state: u64 = 88172645463325252;
	let mut xorshift = |state: &mut u64| -> f64 {
		let mut x = *state;
		x ^= x << 13;
		x ^= x >> 7;
		x ^= x << 17;
		*state = x;
		// 归一化到 [0.0, 1.0)
		(x & 0xFFFFFFFFFFFFF) as f64 / (1u64 << 52) as f64
	};

	eprintln!("[pl] 进入播放循环 (RingBuffer 容量: {} 样本)", ring_capacity);

	// 预分配输出缓冲区（避免热循环中分配）
	let max_bytes = buffer_size * block_align;
	let mut output_buf: Vec<u8> = vec![0u8; max_bytes];
	let mut in_buf: Vec<f64> = vec![0f64; buffer_size * in_channels];
	let mut frame_buf: Vec<f64> = vec![0f64; buffer_size * out_channels];

	// 避免切歌/Seek/曲首出现瞬态爆音：Fade-in
	// - 从 0ms 开始：短淡入（~5ms）避免点击/爆音
	// - 从非 0ms 开始：长淡入（可调）避免恢复/切歌到中间时的瞬态噪音（嚓嚓音）
	let short_fade_ms = unsafe { g_short_fade_ms } as usize;
	let fade_in_total_short = if short_fade_ms == 0 { 0 } else { (output_sample_rate.saturating_mul(short_fade_ms) / 1000).max(1) };
	let resume_fade_ms = unsafe { g_resume_fade_ms } as usize;
	let fade_in_total_resume = (output_sample_rate.saturating_mul(resume_fade_ms) / 1000).max(1);
	let mut fade_in_total = if start_ms == 0 { fade_in_total_short } else { fade_in_total_resume };
	let mut fade_in_pos: usize = 0;

	// Seek：为避免瞬态咔嚓，先短淡出到 0，再执行 seek，并等待解码器完成 seek 后再恢复输出
	let seek_fade_total = fade_in_total_short;
	let mut seek_pending_target_ms: Option<u64> = None;
	let mut seek_fade_out_pos: usize = 0;
	let mut seek_waiting_decode = false;

	let scale = if out_is_float
	{
		1.0
	}
	else
	{
		let b = out_valid_bits.clamp(1, 32);
		if b >= 32 { 2147483647.0 } else { ((1u64 << (b - 1)) - 1) as f64 }
	};
	let out_sample_bytes: usize = if out_is_float
	{
		4
	}
	else
	{
		match out_store_bits
		{
			16 => 2,
			24 => 3,
			32 => 4,
			_ => 2,
		}
	};

	// === 第四步：播放循环 ===
	loop
	{
		// 检查重试请求（来自其他线程：热键/设备通知等）
		let req = take_playback_retry_request();
		if req != RetryReason::None
		{
			let current_ms = current_position_ms(output_sample_rate, channels as usize);
			let resume_ms = if req == RetryReason::Restart { 0 } else { current_ms };

			match req
			{
				RetryReason::ModeChanged => eprintln!("模式切换，当前位置: {}ms", current_ms),
				RetryReason::DefaultDeviceChanged => eprintln!("输出设备变更，当前位置: {}ms", current_ms),
				_ =>
				{}
			}

			set_last_retry_reason(req);
			audio_client.stop_stream().ok();
			stop_decode_task_and_wait();
			return Some(resume_ms);
		}
		if should_abort_playback()
		{
			audio_client.stop_stream().ok();
			stop_decode_task_and_wait();
			return None;
		}

		// 暂停处理：WAIT_TIMEOUT(258) = 已暂停
		if WaitForSingleObject(g_ev_resume, 0) == 258
		{
			set_player_state(PlayerState::Paused);
			eprintln!("[pl] 进入暂停状态");

			// 停止 WASAPI 输出
			audio_client.stop_stream().ok();

			// 如果是独占模式，需要释放设备让其他应用能播放声音
			// 恢复时需要重新初始化，所以返回 Some(current_ms) 让外层重建
			if is_exclusive
			{
				let current_ms = current_position_ms(output_sample_rate, channels as usize);
				stop_decode_task_and_wait();
				eprintln!("[pl] 暂停. 独占模式，释放设备，位置: {}ms", current_ms);
				set_last_retry_reason(RetryReason::PauseExclusiveRelease);
				return Some(current_ms);
			}

			// 共享模式：保持解码线程运行，阻塞等待恢复
			// 解码线程会自己检测暂停事件并阻塞
			loop
			{
				if should_abort_playback()
				{
					stop_decode_task_and_wait();
					return None;
				}

				let hs = [g_ev_pl_quit, g_ev_resume];
				let r = WaitForMultipleObjects(2, hs.as_ptr(), 0, 0xFFFFFFFF);
				if r == 0
				{
					continue;
				}
				if r == 1
				{
					break;
				}

				stop_decode_task_and_wait();
				return None;
			}

			// 恢复播放
			set_player_state(PlayerState::Playing);
			eprintln!("[pl] 恢复播放");
			if let Err(e) = audio_client.start_stream()
			{
				eprintln!("[pl] 恢复 start_stream 失败: {:?}", e);
				let current_ms = current_position_ms(output_sample_rate, channels as usize);
				stop_decode_task_and_wait();
				set_last_retry_reason(RetryReason::ResumeStartStreamFailed);
				return Some(current_ms);
			}
			continue;
		}

		// 处理 Seek 请求（快进/快退 或 UI 绝对 Seek）
		let seek_req_ms = g_seek_req_ms.swap(-1, Ordering::SeqCst);
		let seek_delta = g_to_seek.swap(0, Ordering::SeqCst);

		let mut new_ms_opt: Option<u64> = None;
		let mut seek_delta_for_log: i32 = 0;
		if seek_req_ms >= 0
		{
			new_ms_opt = Some(seek_req_ms as u64);
		}
		else if seek_delta != 0
		{
			seek_delta_for_log = seek_delta;
			let current_ms = current_position_ms(output_sample_rate, channels as usize);
			let mut new_ms = if seek_delta > 0
			{
				current_ms.saturating_add(seek_delta as u64 * 1000)
			}
			else
			{
				current_ms.saturating_sub((-seek_delta) as u64 * 1000)
			};
			new_ms_opt = Some(new_ms);
		}

		if let Some(mut new_ms) = new_ms_opt
		{
			let current_ms = current_position_ms(output_sample_rate, channels as usize);

			if duration_ms > 0 && new_ms >= duration_ms
			{
				if seek_delta_for_log != 0
				{
					eprintln!("[pl] Seek[{}]: {}ms -> {}ms (超过曲末，下一首)", seek_delta_for_log, current_ms, new_ms);
				}
				else
				{
					eprintln!("[pl] SeekTo: {}ms -> {}ms (超过曲末，下一首)", current_ms, new_ms);
				}
				g_to_next.store(true, Ordering::SeqCst);
				continue;
			};

			if seek_delta_for_log != 0
			{
				eprintln!("[pl] Seek[{}]: {}ms -> {}ms", seek_delta_for_log, current_ms, new_ms);
			}
			else
			{
				eprintln!("[pl] SeekTo: {}ms -> {}ms", current_ms, new_ms);
			}

			if seek_waiting_decode
			{
				// 已在等待解码器完成 seek（此时输出静音）：直接更新目标位置
				consumer.clear();
				SetEvent(g_ev_ring_space);
				g_seek_to_ms.store(new_ms as i64, Ordering::SeqCst);
				SetEvent(g_ev_dec_wakeup);
				fade_in_pos = 0;
				fade_in_total = fade_in_total_short;
				SAMPLES_PLAYED.store((new_ms * output_sample_rate as u64 * channels as u64) / 1000, Ordering::SeqCst);
				last_progress_ms = new_ms;
				PostMessageW(G_HWND, WM_PROGRESS, new_ms as usize, duration_ms as i64);

				seek_pending_target_ms = None;
				seek_fade_out_pos = 0;
			}
			else
			{
				// 进入淡出阶段：先把当前输出拉到 0，避免“旧波形 -> 静音/新位置”产生点击
				if seek_pending_target_ms.is_none()
				{
					seek_fade_out_pos = 0;
				}
				seek_pending_target_ms = Some(new_ms);
			}
		}

		// 等待 WASAPI 事件
		if event.wait_for_event(1000).is_err()
		{
			continue;
		}

		let frames_available = if is_exclusive
		{
			buffer_size
		}
		else
		{
			let padding = audio_client
				.get_current_padding()
				.unwrap_or(0) as usize;
			buffer_size.saturating_sub(padding)
		};

		if frames_available == 0
		{
			continue;
		}

		let in_samples_needed = frames_available * in_channels;
		let out_samples_needed = frames_available * out_channels;
		let bytes_needed = frames_available * block_align;

		// Seek 等待阶段：在解码器完成 seek 前持续输出静音，并丢弃旧缓冲，避免把旧音频残片播出来
		if seek_waiting_decode
		{
			if g_seek_just.swap(false, Ordering::SeqCst)
			{
				consumer.clear();
				seek_waiting_decode = false;
				fade_in_pos = 0;
				fade_in_total = fade_in_total_short;

				let current_ms = current_position_ms(output_sample_rate, channels as usize);
				last_progress_ms = current_ms;
			}
			else
			{
				consumer.clear();
				SetEvent(g_ev_ring_space);
				output_buf[..bytes_needed].fill(0);
				if write_or_fail!(render_client, frames_available, &output_buf[..bytes_needed])
				{
					let current_ms = current_position_ms(output_sample_rate, channels as usize);
					audio_client.stop_stream().ok();
					stop_decode_task_and_wait();
					return Some(current_ms);
				}
				continue;
			}
		}

		// 检查 decode 线程是否刚完成 seek（解决竞态条件）
		if g_seek_just.swap(false, Ordering::SeqCst)
		{
			consumer.clear();
			let current_ms = current_position_ms(output_sample_rate, channels as usize);
			last_progress_ms = current_ms;
		}

		// 从 RingBuffer 读取样本
		let available = consumer.occupied_len();
		let volume = g_to_volume.load(Ordering::SeqCst) as f64 / 100.0; // f64 精度

		let read_samples = if available >= in_samples_needed
		{
			in_samples_needed
		}
		else if available > 0
		{
			// RingBuffer 不足：只有在已到达曲末（EOS 标记已写入）时才消费剩余样本；否则输出静音等待缓冲填充
			let (left, right) = consumer.as_slices();
			let has_eos = left
				.iter()
				.any(|s| s.to_bits() == EOS_MARKER_BITS)
				|| right
					.iter()
					.any(|s| s.to_bits() == EOS_MARKER_BITS);
			if has_eos { available } else { 0 }
		}
		else
		{
			0
		};

		if read_samples == 0
		{
			// 欠载：输出静音帧（避免爆音）
			output_buf[..bytes_needed].fill(0);
			if write_or_fail!(render_client, frames_available, &output_buf[..bytes_needed])
			{
				// 写入失败，清理当前上下文并返回
				let current_ms = current_position_ms(output_sample_rate, channels as usize);
				audio_client.stop_stream().ok();
				stop_decode_task_and_wait();
				return Some(current_ms);
			}

			// 若此时有待处理 seek：当前已经在静音，直接执行 seek（减少交互延迟）
			if let Some(target_ms) = seek_pending_target_ms
			{
				consumer.clear();
				SetEvent(g_ev_ring_space);
				g_seek_to_ms.store(target_ms as i64, Ordering::SeqCst);
				SetEvent(g_ev_dec_wakeup);
				seek_waiting_decode = true;
				seek_pending_target_ms = None;
				seek_fade_out_pos = 0;

				fade_in_pos = 0;
				fade_in_total = fade_in_total_short;
				SAMPLES_PLAYED.store((target_ms * output_sample_rate as u64 * channels as u64) / 1000, Ordering::SeqCst);
				last_progress_ms = target_ms;
				PostMessageW(G_HWND, WM_PROGRESS, target_ms as usize, duration_ms as i64);
			}
			continue;
		}

		// 正常情况 / EOS 收尾：消费 read_samples 样本（不足一帧则其余补零）
		consumer.pop_slice(&mut in_buf[..read_samples]);

		// 通知 decode 线程有空间可用（~20μs 精度）
		SetEvent(g_ev_ring_space);

		let mut hit_eos = false;
		let mut valid_samples = read_samples;
		for i in 0..read_samples
		{
			let bits = in_buf[i].to_bits();
			if bits == EOS_MARKER_BITS
			{
				hit_eos = true;
				valid_samples = i;
				in_buf[i..in_samples_needed].fill(0.0);
				break;
			}
			if !in_buf[i].is_finite()
			{
				in_buf[i] = 0.0;
			}
		}
		if !hit_eos && read_samples < in_samples_needed
		{
			// 剩余部分填静音
			in_buf[read_samples..in_samples_needed].fill(0.0);
		}

		// 声道转换（输入 RingBuffer 永远使用解码器声道数）
		if in_channels == out_channels
		{
			frame_buf[..out_samples_needed].copy_from_slice(&in_buf[..in_samples_needed]);
		}
		else if in_channels == 1
		{
			for f in 0..frames_available
			{
				let s = in_buf[f];
				let base = f * out_channels;
				frame_buf[base..base + out_channels].fill(s);
			}
		}
		else if out_channels == 1
		{
			for f in 0..frames_available
			{
				let base = f * in_channels;
				let mut sum = 0.0;
				for c in 0..in_channels
				{
					sum += in_buf[base + c];
				}
				frame_buf[f] = sum / (in_channels as f64);
			}
		}
		else
		{
			frame_buf[..out_samples_needed].fill(0.0);
			let min_ch = in_channels.min(out_channels);
			for f in 0..frames_available
			{
				let in_base = f * in_channels;
				let out_base = f * out_channels;
				frame_buf[out_base..out_base + min_ch].copy_from_slice(&in_buf[in_base..in_base + min_ch]);
			}
		}

		// 更新已播放样本计数
		SAMPLES_PLAYED.fetch_add(valid_samples as u64, Ordering::SeqCst);

		// 检查是否需要发送进度通知
		if valid_samples > 0
		{
			let current_ms = current_position_ms(output_sample_rate, channels as usize);
			if current_ms >= last_progress_ms + PROGRESS_NOTIFY_INTERVAL_MS
			{
				// wparam=当前位置(l), lparam=总时长(w)
				PostMessageW(G_HWND, WM_PROGRESS, current_ms as usize, duration_ms as i64);
				last_progress_ms = current_ms;
			}
		}

		// 曲首/Seek 后短暂淡入，避免点击/爆音
		if fade_in_pos < fade_in_total
		{
			for f in 0..frames_available
			{
				if fade_in_pos >= fade_in_total
				{
					break;
				}
				let g = ((fade_in_pos + 1) as f64 / fade_in_total as f64).min(1.0);
				let base = f * out_channels;
				for ch in 0..out_channels
				{
					frame_buf[base + ch] *= g;
				}
				fade_in_pos += 1;
			}
		}

		// Seek：短淡出到 0，避免 seek 时产生瞬态点击
		let mut seek_commit_ms: Option<u64> = None;
		let mut seek_silence_from_frame: Option<usize> = None;
		if let Some(target_ms) = seek_pending_target_ms
		{
			let total = seek_fade_total.max(1);
			for f in 0..frames_available
			{
				if seek_fade_out_pos < total
				{
					let g = 1.0 - ((seek_fade_out_pos + 1) as f64 / total as f64);
					let g = g.max(0.0);
					let base = f * out_channels;
					for ch in 0..out_channels
					{
						frame_buf[base + ch] *= g;
					}
					seek_fade_out_pos += 1;
					if seek_fade_out_pos >= total
					{
						seek_silence_from_frame = Some(f);
					}
				}
				else
				{
					let base = f * out_channels;
					frame_buf[base..base + out_channels].fill(0.0);
				}
			}

			if seek_fade_out_pos >= total
			{
				seek_commit_ms = Some(target_ms);
				seek_pending_target_ms = None;
			}
		}

		// EOS：尾部短淡出，避免瞬态噪声
		if hit_eos && valid_samples > 0
		{
			let valid_frames = valid_samples / in_channels;
			let fade_out_total = if short_fade_ms == 0
			{
				0
			}
			else
			{
				(output_sample_rate.saturating_mul(short_fade_ms) / 1000)
					.max(1)
					.min(valid_frames)
			};
			if fade_out_total > 0
			{
				let start_frame = valid_frames.saturating_sub(fade_out_total);
				let denom = if fade_out_total > 1 { (fade_out_total - 1) as f64 } else { 1.0 };
				for i in 0..fade_out_total
				{
					let g = 1.0 - (i as f64 / denom);
					let base = (start_frame + i) * out_channels;
					for c in 0..out_channels
					{
						frame_buf[base + c] *= g;
					}
				}
			}
		}

		let valid_out_samples = (valid_samples / in_channels) * out_channels;

		// 转换格式并写入输出缓冲
		for (i, &sample) in frame_buf[..out_samples_needed]
			.iter()
			.enumerate()
		{
			let byte_offset = (i / out_channels) * block_align + (i % out_channels) * (out_store_bits / 8);

			// EOS 之后的补齐静音：不要做 dither，确保真正输出 0，避免尾部“刺啦”噪声
			if i >= valid_out_samples
			{
				output_buf[byte_offset..byte_offset + out_sample_bytes].fill(0);
				continue;
			}

			// Seek 淡出完成后的静音尾段：不要做 dither，确保真正输出 0
			if let Some(from_frame) = seek_silence_from_frame
			{
				let frame_idx = i / out_channels;
				if frame_idx >= from_frame
				{
					output_buf[byte_offset..byte_offset + out_sample_bytes].fill(0);
					continue;
				}
			}

			if out_is_float
			{
				// Float 输出：f64 -> f32（WASAPI float 是 32 位）
				let s = ((sample * volume).clamp(-1.0, 1.0)) as f32;
				let bytes = s.to_le_bytes();
				output_buf[byte_offset..byte_offset + 4].copy_from_slice(&bytes);
			}
			else
			{
				// Int 需要 dither
				// TPDF Dither: +/- 1 LSB triangular distribution
				// dither 幅度 = 1 LSB，即 1.0/scale
				let dither = if out_valid_bits < 32
				{
					(xorshift(&mut rng_state) - xorshift(&mut rng_state)) // [-1, 1) TPDF
				}
				else
				{
					0.0
				};

				// 应用音量 + 缩放 + dither（dither 不需要缩放，已经是 1 LSB）
				let scaled = (sample * volume).clamp(-1.0, 1.0) * scale + dither;

				let b = out_valid_bits.clamp(1, 32);
				let (int_min, int_max) = if b >= 32
				{
					(i32::MIN as i64, i32::MAX as i64)
				}
				else
				{
					let p = 1i64 << (b - 1);
					(-p, p - 1)
				};

				// Round to nearest integer, then clamp to avoid overflow wrap (重要：dither 可能把 max+0.5 推到 max+1)
				let mut s = scaled.round() as i64;
				s = s.clamp(int_min, int_max);

				// 对 storebits > validbits 的情况（典型 24-in-32），按规范左对齐有效位，保持 LSB 为 0
				let shift = out_store_bits.saturating_sub(b);
				if shift > 0
				{
					s <<= shift;
				}

				let s = s as i32;

				match out_store_bits
				{
					16 =>
					{
						let bytes = (s as i16).to_le_bytes();
						output_buf[byte_offset..byte_offset + 2].copy_from_slice(&bytes);
					}
					24 =>
					{
						let bytes = s.to_le_bytes();
						output_buf[byte_offset..byte_offset + 3].copy_from_slice(&[bytes[0], bytes[1], bytes[2]]);
					}
					32 =>
					{
						let bytes = s.to_le_bytes();
						output_buf[byte_offset..byte_offset + 4].copy_from_slice(&bytes);
					}
					_ =>
					{
						let bytes = (s as i16).to_le_bytes();
						output_buf[byte_offset..byte_offset + 2].copy_from_slice(&bytes);
					}
				}
			}
		}

		if write_or_fail!(render_client, frames_available, &output_buf[..bytes_needed])
		{
			// 写入失败，清理当前上下文并返回，让 player_thread 重建新上下文重试
			let current_ms = current_position_ms(output_sample_rate, channels as usize);
			audio_client.stop_stream().ok();
			stop_decode_task_and_wait();
			return Some(current_ms);
		}

		// Seek commit：淡出后再执行 seek（避免点击/残片）
		if let Some(target_ms) = seek_commit_ms
		{
			consumer.clear();
			SetEvent(g_ev_ring_space);
			g_seek_to_ms.store(target_ms as i64, Ordering::SeqCst);
			SetEvent(g_ev_dec_wakeup);
			seek_waiting_decode = true;
			seek_fade_out_pos = 0;

			fade_in_pos = 0;
			fade_in_total = fade_in_total_short;
			SAMPLES_PLAYED.store((target_ms * output_sample_rate as u64 * channels as u64) / 1000, Ordering::SeqCst);
			last_progress_ms = target_ms;
			PostMessageW(G_HWND, WM_PROGRESS, target_ms as usize, duration_ms as i64);
			continue;
		}

		if hit_eos
		{
			eprintln!("播放完成");

			let padding = audio_client
				.get_current_padding()
				.unwrap_or(0) as usize;
			if padding > 0 && output_sample_rate > 0
			{
				let drain_ms = ((padding as u64) * 1000 / (output_sample_rate as u64))
					.saturating_add(10)
					.min(500);
				Sleep(drain_ms as u32);
			}

			audio_client.stop_stream().ok();
			stop_decode_task_and_wait();
			return None;
		}
	}
}

// src\tray.rs
const TRAY_ICON_PATH: &str = r"d:\float\OneDrive\diatom\conf\icon\software\fm.ico";
const TRAY_TOOLTIP: &str = "fog";
const TRAY_UID: u32 = 1;

static mut g_wm_taskbar_created: u32 = 0;

static mut g_tray_icon: i64 = 0;
static mut g_tray_added: bool = false;
static mut g_is_load_tray: bool = true;
static mut g_tray_hwnd: i64 = 0;
static mut g_tray_tip: [u16; 128] = [0; 128];

unsafe fn tray_recreate(hwnd: i64) {
	if hwnd == 0
	{
		return;
	}

	// Explorer restarted -> notification area recreated -> our icon is lost.
	// Delete any leftover icon entry first to avoid duplicates, then re-add.
	let mut nid: NOTIFYICONDATAW = zeroed();
	nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
	nid.hWnd = hwnd;
	nid.uID = TRAY_UID;
	Shell_NotifyIconW(NIM_DELETE, &mut nid);

	g_tray_added = false;
	tray_add(hwnd);
}

unsafe fn tray_add(hwnd: i64) {
	if g_tray_added
	{
		return;
	}

	let hicon = tray_load_icon();

	if hicon == 0
	{
		g_is_load_tray = false;
		return;
	}

	let mut nid: NOTIFYICONDATAW = zeroed();
	nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
	nid.hWnd = hwnd;
	nid.uID = TRAY_UID;
	nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
	nid.uCallbackMessage = WM_TRAYICON;
	nid.hIcon = hicon;

	// Keep the last tooltip so it can be restored after Explorer restarts.
	if g_tray_tip[0] == 0
	{
		copy_to_wide_buf(&mut g_tray_tip, TRAY_TOOLTIP);
	}
	nid.szTip = g_tray_tip;

	if Shell_NotifyIconW(NIM_ADD, &mut nid) == 0
	{
		eprintln!("[tray] Shell_NotifyIconW(NIM_ADD) failed: err={}", GetLastError());
		g_is_load_tray = false;
		return;
	}

	nid.uTimeoutOrVersion = NOTIFYICON_VERSION_4;
	Shell_NotifyIconW(NIM_SETVERSION, &mut nid);

	g_tray_added = true;
	g_tray_hwnd = hwnd;
	g_is_load_tray = true;
	eprintln!("[tray] added: icon={}, hwnd={}", TRAY_ICON_PATH, hwnd);
}

unsafe fn tray_load_icon() -> i64 {
	if g_tray_icon != 0
	{
		return g_tray_icon;
	}

	let hicon = LoadImageW(0, to_wstring(TRAY_ICON_PATH).as_ptr(), IMAGE_ICON, 0, 0, LR_LOADFROMFILE | LR_DEFAULTSIZE);
	if hicon == 0
	{
		eprintln!("[tray] LoadImageW failed: path={}, err={}", TRAY_ICON_PATH, GetLastError());
		return 0;
	}

	g_tray_icon = hicon;
	hicon
}

unsafe fn tray_apply_window_icon(hwnd: i64) {
	if hwnd == 0
	{
		return;
	}

	let hicon = tray_load_icon();
	if hicon == 0
	{
		return;
	}

	SendMessageW(hwnd, WM_SETICON, ICON_SMALL, hicon);
	SendMessageW(hwnd, WM_SETICON, ICON_BIG, hicon);
}

unsafe fn tray_set_tooltip(tip: &str) {
	copy_to_wide_buf(&mut g_tray_tip, tip);

	let mut nid: NOTIFYICONDATAW = zeroed();
	nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
	nid.hWnd = g_tray_hwnd;
	nid.uID = TRAY_UID;
	// Keep NIF_SHOWTIP enabled, otherwise Windows may stop showing the tooltip.
	nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
	nid.uCallbackMessage = WM_TRAYICON;
	nid.hIcon = tray_load_icon();
	nid.szTip = g_tray_tip;

	if Shell_NotifyIconW(NIM_MODIFY, &mut nid) == 0
	{
		eprintln!("[tray] Shell_NotifyIconW(NIM_MODIFY tip) failed: err={}", GetLastError());
	}
}

unsafe fn tray_remove(hwnd: i64) {
	if g_tray_added && hwnd != 0
	{
		let mut nid: NOTIFYICONDATAW = zeroed();
		nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
		nid.hWnd = hwnd;
		nid.uID = TRAY_UID;
		Shell_NotifyIconW(NIM_DELETE, &mut nid);
	}

	if g_tray_icon != 0
	{
		DestroyIcon(g_tray_icon);
		g_tray_icon = 0;
	}

	g_tray_added = false;
	g_tray_hwnd = 0;
	eprintln!("[tray] removed");
}

unsafe fn tray_on_message(_hwnd: i64, wparam: usize, lparam: i64) {
	let uid = wparam as u32;
	let ev_raw = lparam as u32;
	let ev = ev_raw & 0xFFFF;

	// Notification icon v4 uses NIN_* messages (WM_USER + n) instead of the legacy WM_* mouse messages.
	const NIN_SELECT: u32 = WM_USER + 0;
	const NIN_KEYSELECT: u32 = WM_USER + 1;
	const NIN_BALLOONSHOW: u32 = WM_USER + 2;
	const NIN_BALLOONHIDE: u32 = WM_USER + 3;
	const NIN_BALLOONTIMEOUT: u32 = WM_USER + 4;
	const NIN_BALLOONUSERCLICK: u32 = WM_USER + 5;
	const NIN_POPUPOPEN: u32 = WM_USER + 6;
	const NIN_POPUPCLOSE: u32 = WM_USER + 7;

	if ev == WM_LBUTTONUP
	//WM_LBUTTONDBLCLK || ev == NIN_BALLOONUSERCLICK
	{
		PostMessageW(_hwnd, WM_TOGGLE_WINDOW, 0, 0);
	}

	/*
	let name = match ev
	{
					WM_MOUSEMOVE =>
					{
									return;
									// "mousemove"
					}
					NIN_SELECT => "select",
					NIN_KEYSELECT => "keyselect",
					NIN_BALLOONSHOW => "balloon_show",
					NIN_BALLOONHIDE => "balloon_hide",
					NIN_BALLOONTIMEOUT => "balloon_timeout",
					NIN_BALLOONUSERCLICK => "balloon_userclick",
					NIN_POPUPOPEN => "popup_open",
					NIN_POPUPCLOSE => "popup_close",

					WM_LBUTTONDOWN => "lbutton_down",
					WM_LBUTTONUP => "lbutton_up",
					WM_LBUTTONDBLCLK => "lbutton_dblclk",
					WM_RBUTTONDOWN => "rbutton_down",
					WM_RBUTTONUP => "rbutton_up",
					WM_RBUTTONDBLCLK => "rbutton_dblclk",
					WM_MBUTTONDOWN => "mbutton_down",
					WM_MBUTTONUP => "mbutton_up",
					WM_MBUTTONDBLCLK => "mbutton_dblclk",
					WM_CONTEXTMENU => "contextmenu",
					_ => "unknown",
	};

	if name == "unknown"
	{
					//eprintln!("[tray] event: uid={}, ev=0x{:x} (raw=0x{:x})", uid, ev, ev_raw);
	}
	else
	{
					eprintln!("[tray] event: uid={}, {}", uid, name);

					// 双击或者气泡点击 -> 切换窗口显隐
					if ev == WM_LBUTTONDBLCLK || ev == NIN_BALLOONUSERCLICK
					{
									PostMessageW(_hwnd, WM_TOGGLE_WINDOW, 0, 0);
					}
	}
					*/
}

fn copy_to_wide_buf(dst: &mut [u16], s: &str) {
	dst.fill(0);
	for (i, ch) in s
		.encode_utf16()
		.take(dst.len().saturating_sub(1))
		.enumerate()
	{
		dst[i] = ch;
	}
}

// src\ui.rs
static mut g_ui_is_visible: bool = false;

/// 创建并显示 UI 窗口
unsafe fn ui_create_window() {
	// 如果窗口已存在，直接显示
	if UI_HWND != 0
	{
		ShowWindow(UI_HWND, SW_SHOW);
		g_ui_is_visible = true;
		return;
	}

	let hinstance = GetModuleHandleW(null_mut());
	let cursor = LoadCursorW(0, IDC_ARROW);

	// 注册窗口类
	let wc = WNDCLASSW {
		style: CS_HREDRAW | CS_VREDRAW,
		lpfnWndProc: ui_window_proc,
		cbClsExtra: 0,
		cbWndExtra: 0,
		hInstance: hinstance,
		hIcon: 0,
		hCursor: cursor,
		hbrBackground: COLOR_WINDOW + 1,
		lpszMenuName: null_mut(),
		lpszClassName: UI_CLASS_NAME.as_ptr(),
	};
	RegisterClassW(&wc);

	// 创建窗口
	let title = to_wstring("Fog Player - 播放列表与日志");
	UI_HWND = CreateWindowExW(
		0,
		UI_CLASS_NAME.as_ptr(),
		title.as_ptr(),
		WS_OVERLAPPEDWINDOW,
		CW_USEDEFAULT,
		CW_USEDEFAULT,
		600,
		550,
		0,
		0,
		hinstance,
		0,
	);

	if UI_HWND == 0
	{
		eprintln!("错误: 创建 UI 窗口失败.\n程序将退出");
		msg_box("错误: 创建 UI 窗口失败.\n程序将退出", "错误", MB_ICONERROR | MB_OK);
		PostMessageW(G_HWND, WM_DESTROY, 0, 0);
		return;
	}

	// Use the same icon as the tray icon (title bar + taskbar).
	tray_apply_window_icon(UI_HWND);

	// Window is created here; show/hide happens after restore to avoid startup flicker.
	ui_log_request_flush();
}

unsafe fn ui_fit_playlist_columns_to_client_width(hlist: i64) {
	if hlist == 0 || UI_PLAYLIST_COL_COUNT_TOTAL == 0
	{
		return;
	}

	let mut rc: RECT = zeroed();
	if GetClientRect(hlist, &mut rc) == 0
	{
		return;
	}

	let total_w = (rc.right - rc.left).max(0);
	if total_w <= 0
	{
		return;
	}

	let last = UI_PLAYLIST_COL_COUNT_TOTAL.saturating_sub(1);
	let mut sum = 0i32;
	let mut last_w = 0i32;
	for i in 0..UI_PLAYLIST_COL_COUNT_TOTAL
	{
		let w = SendMessageW(hlist, LVM_GETCOLUMNWIDTH, i, 0) as i32;
		let w = w.max(0);
		if i == last
		{
			last_w = w;
		}
		sum = sum.saturating_add(w);
	}

	if sum <= total_w || last_w <= 0
	{
		return;
	}

	let overflow = sum - total_w;
	let new_last = (last_w - overflow).max(1);
	if new_last != last_w
	{
		SendMessageW(hlist, LVM_SETCOLUMNWIDTH, last, new_last as i64);
	}
}

unsafe fn do_playlist_update(hlist: i64, items: &[SongInfo]) {
	let count = items.len();
	SendMessageW(hlist, LVM_SETITEMCOUNT, count, LVSICF_NOSCROLL as i64);
}

unsafe fn ui_copy_text_to_buffer(dst: *mut u16, max_len: i32, text: &str) {
	if dst.is_null() || max_len <= 0
	{
		return;
	}

	let max_len = max_len as usize;
	let mut i = 0usize;
	for ch in text
		.encode_utf16()
		.take(max_len.saturating_sub(1))
	{
		*dst.add(i) = ch;
		i += 1;
	}
	*dst.add(i) = 0;
}

unsafe fn ui_playlist_cell_text(li_id: usize, item_index: usize, sub_item: i32) -> Option<String> {
	let pool = m_pl_pool.read().ok()?;
	let items = pool.get(&li_id)?;
	let song = items.get(item_index)?;

	let text = match sub_item
	{
		0 =>
		{
			if UI_CURRENT_PLAYING_LI_ID == li_id && UI_CURRENT_PLAYING_IDX == item_index as i32
			{
				format!("{} ▶", item_index + 1)
			}
			else
			{
				format!("{}", item_index + 1)
			}
		}
		1 =>
		{
			let path = song.path.as_str();
			let filename = path
				.rsplit(|c| c == '\\' || c == '/')
				.next()
				.unwrap_or(path);
			let title = if song.title.is_empty() { filename } else { song.title.as_str() };
			title.to_string()
		}
		2 =>
		{
			if !song.duration_text.is_empty()
			{
				song.duration_text.clone()
			}
			else if song.duration_ms > 0
			{
				format_time(song.duration_ms)
			}
			else
			{
				"--:--".to_string()
			}
		}
		3 =>
		{
			if song.author.is_empty()
			{
				"-".to_string()
			}
			else
			{
				song.author.to_string()
			}
		}
		4 =>
		{
			if song.album.is_empty()
			{
				"-".to_string()
			}
			else
			{
				song.album.to_string()
			}
		}
		UI_PLAYLIST_INFO_COL_SUBITEM => ui_info_col_text(song),
		_ => String::new(),
	};

	Some(text)
}

fn ui_playlist_filename(path: &str) -> &str {
	path.rsplit(|c| c == '\\' || c == '/')
		.next()
		.unwrap_or(path)
}

fn ui_playlist_title_for_sort(song: &SongInfo) -> &str {
	let title = song.title.trim();
	if !title.is_empty()
	{
		return title;
	}
	ui_playlist_filename(&song.path)
}

fn ui_playlist_cmp_text(a: &str, b: &str) -> std::cmp::Ordering {
	let a = a.trim();
	let b = b.trim();
	let ae = a.is_empty();
	let be = b.is_empty();
	if ae != be
	{
		// Empty values last.
		return ae.cmp(&be);
	}
	if ae
	{
		return std::cmp::Ordering::Equal;
	}
	cmp_ascii_case_insensitive(a, b)
}

fn ui_playlist_cmp_duration(a: u64, b: u64) -> std::cmp::Ordering {
	let a = if a == 0 { u64::MAX } else { a };
	let b = if b == 0 { u64::MAX } else { b };
	a.cmp(&b)
}

fn ui_playlist_sort_compare(col: i32, a: &SongInfo, b: &SongInfo) -> std::cmp::Ordering {
	use std::cmp::Ordering;

	let ord = match col
	{
		1 => ui_playlist_cmp_text(ui_playlist_title_for_sort(a), ui_playlist_title_for_sort(b)),
		2 => ui_playlist_cmp_duration(a.duration_ms, b.duration_ms),
		3 => ui_playlist_cmp_text(&a.author, &b.author),
		4 => ui_playlist_cmp_text(&a.album, &b.album),
		_ => ui_playlist_cmp_text(&a.path, &b.path),
	};

	if ord == Ordering::Equal { ui_playlist_cmp_text(&a.path, &b.path) } else { ord }
}

unsafe fn ui_playlist_sort_by_column(li_id: usize, col: i32, asc: bool) {
	let playing_path_key = NOW_PLAYING
		.read()
		.ok()
		.map(|np| normalize_path_key(&np.path))
		.unwrap_or_default();

	let mut pool = m_pl_pool.write().unwrap();
	let Some(items) = pool.get_mut(&li_id)
	else
	{
		return;
	};

	if items.len() <= 1
	{
		return;
	}

	items.sort_by(|a, b| {
		let ord = ui_playlist_sort_compare(col, a, b);
		if asc { ord } else { ord.reverse() }
	});

	let _ = db_replace_playlist(li_id, None, items);

	// Keep "now playing" marker aligned when sorting the active playlist.
	if li_id == g_li_id.load(Ordering::SeqCst) && !playing_path_key.is_empty()
	{
		if let Some(new_idx) = items
			.iter()
			.position(|s| normalize_path_key(&s.path) == playing_path_key)
		{
			g_track.store(new_idx, Ordering::SeqCst);
			UI_CURRENT_PLAYING_LI_ID = li_id;
			UI_CURRENT_PLAYING_IDX = new_idx as i32;
			ui_playlist_select(li_id, new_idx);
		}
	}
}

unsafe fn ui_fill_playlist_dispinfo(li_id: usize, item: &mut LVITEMW) {
	if item.iItem < 0
	{
		return;
	}

	let text = ui_playlist_cell_text(li_id, item.iItem as usize, item.iSubItem).unwrap_or_default();
	ui_copy_text_to_buffer(item.pszText as *mut u16, item.cchTextMax, &text);
}

/// 更新播放列表
/// 传入完整的播放列表，会自动提取文件名显示
unsafe fn ui_playlist_update(li_id: usize, items: Vec<SongInfo>) {
	let boxed = Box::new(items);
	let ptr = Box::into_raw(boxed);
	PostMessageW(UI_HWND, WM_UI_PLAYLIST_UPDATE, li_id, ptr as i64);
}

/// 同步播放列表选项卡显示
unsafe fn ui_sync_playlist_tabs(li_id: usize) {
	PostMessageW(UI_HWND, WM_UI_TAB_SYNC, li_id, 0);
}

/// 选中播放列表中的某一项
/// idx: 要选中的索引 (0-based)
unsafe fn ui_playlist_select(li_id: usize, idx: usize) {
	PostMessageW(UI_HWND, WM_UI_PLAYLIST_SELECT, li_id, idx as i64);
}

/// 设置当前播放的歌曲信息 并在列表中高亮显示当前播放项

unsafe fn ui_set_now_playing(li_id: usize, idx: usize, track_path: &str, title: &str, author: &str) {
	PostMessageW(UI_HWND, WM_UI_NOW_PLAYING, li_id, idx as i64);
	// 更新窗口标题
	let window_title = if author.is_empty() { title.to_owned() } else { format!("{} - {}", title, author) };
	let ws = to_wstring(&window_title);
	SetWindowTextW(UI_HWND, ws.as_ptr());

	if g_is_load_tray
	{
		tray_set_tooltip(&window_title);
	};

	//ui_cover_on_track_change(track_path, title, author);
}

/// 切换窗口显隐状态
unsafe fn ui_toggle_visibility() {
	// is_visible
	if (GetWindowLongW(UI_HWND, -16) as u32 & WS_VISIBLE) != 0
	{
		ShowWindow(UI_HWND, 0); // SW_HIDE = 0
		g_ui_is_visible = false;
	}
	else
	{
		ShowWindow(UI_HWND, SW_SHOW);
		SetForegroundWindow(UI_HWND);
		g_ui_is_visible = true;

		// 窗口显示时才需要进度提示：这里顺带在切换显隐后做一次同步，
		// 避免“隐藏期间状态变了/暂停无进度消息”导致任务栏不刷新。
		let state = get_player_state();
		taskbar_sync_player_state(state);

		let total_ms = TRACK_DURATION_MS.load(Ordering::Relaxed);

		if total_ms > 0
		{
			let sample_rate = OUTPUT_SAMPLE_RATE.load(Ordering::Relaxed);
			let channels = OUTPUT_CHANNELS.load(Ordering::Relaxed);
			if sample_rate > 0 && channels > 0
			{
				let pos_ms = current_position_ms(sample_rate, channels);
				taskbar_set_pos(g_TBL_PTR, UI_HWND, pos_ms, total_ms);
			};
		};
	};
	ui_save_window_state();
}

/// 设置窗口可见性
unsafe fn ui_set_visible(visible: bool) {
	ShowWindow(UI_HWND, if visible { SW_SHOWDEFAULT } else { 0 });
	g_ui_is_visible = visible;
}

/// 设置窗口位置和大小
unsafe fn ui_set_window_rect(x: i32, y: i32, w: i32, h: i32) {
	// 使用 SetWindowPos 设置位置，不改变 Z 序 (HWND_TOP = 0)
	SetWindowPos(UI_HWND, 0, x, y, w, h, 0x0014); // SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW? NO
	// 0x0010 = SWP_NOACTIVATE, 0x0004 = SWP_NOZORDER
	// 如果只想移动不显示，不要加 SWP_SHOWWINDOW (0x0040)
}

/// 保存窗口状态到数据库
unsafe fn ui_save_window_state() {
	let is_visible = g_ui_is_visible;

	// 获取窗口矩形
	let mut rect: RECT = zeroed();
	if GetWindowRect(UI_HWND, &mut rect) != 0
	{
		let w = rect.right - rect.left;
		let h = rect.bottom - rect.top;
		db_save_window_state(is_visible, rect.left, rect.top, w, h);
	}
}

/// 刷新音乐库 TreeView
unsafe fn ui_tree_refresh() {
	PostMessageW(UI_HWND, WM_UI_TREE_REFRESH, 0, 0);
}

/// 同步音量滑块
unsafe fn ui_volume_sync(volume: u32) {
	PostMessageW(UI_HWND, WM_UI_VOLUME_SYNC, volume as usize, 0);
}

/// 同步播放状态指示
unsafe fn ui_play_state_sync(is_playing: bool) {
	PostMessageW(UI_HWND, WM_UI_PLAY_STATE, if is_playing { 1 } else { 0 }, 0);
}

// src\ui_cover.rs
// Tree 右侧封面控件：内存图片 -> GDI+ 解码 -> 渲染

struct UiCoverState {
	pbitmap: i64,
	img_w: u32,
	img_h: u32,
}

static UI_COVER_STATE: LazyLock<Mutex<UiCoverState>> = LazyLock::new(|| Mutex::new(UiCoverState { pbitmap: 0, img_w: 0, img_h: 0 }));

static mut UI_GDIPLUS_TOKEN: usize = 0;

static mut g_cover_is_show: bool = false;

static UI_COVER_REQ_SEQ: AtomicU64 = AtomicU64::new(0);

static g_cover_last_path: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));

unsafe fn ui_cover_com_release(p: i64) {
	if p == 0
	{
		return;
	}
	let vtbl = *(p as *const *const usize);
	let release: unsafe extern "system" fn(i64) -> u32 = transmute(*vtbl.add(2));
	release(p);
}

unsafe fn ui_cover_paint(hwnd: i64) {
	let mut ps: PAINTSTRUCT = zeroed();
	let hdc = BeginPaint(hwnd, &mut ps);
	if hdc == 0
	{
		return;
	}

	let mut rc: RECT = zeroed();
	GetClientRect(hwnd, &mut rc);
	let w = (rc.right - rc.left).max(0);
	let h = (rc.bottom - rc.top).max(0);

	let white = GetStockObject(0); // WHITE_BRUSH
	if white != 0
	{
		FillRect(hdc, &rc as *const RECT, white);
	}

	let (pbitmap, img_w, img_h) = {
		let s = UI_COVER_STATE.lock().unwrap();
		(s.pbitmap, s.img_w, s.img_h)
	};

	if pbitmap != 0 && w > 0 && h > 0 && img_w > 0 && img_h > 0
	{
		let mut g: i64 = 0;
		if GdipCreateFromHDC(hdc, &mut g) == 0 && g != 0
		{
			// High quality scale
			GdipSetInterpolationMode(g, 7); // HighQualityBicubic
			GdipSetPixelOffsetMode(g, 2); // HighQuality
			GdipSetSmoothingMode(g, 2); // HighQuality
			GdipSetCompositingQuality(g, 2); // HighQuality

			let iw = img_w as f64;
			let ih = img_h as f64;
			let dw = w as f64;
			let dh = h as f64;
			let scale = (dw / iw).min(dh / ih).max(0.0);
			let draw_w = (iw * scale).round().clamp(0.0, dw) as i32;
			let draw_h = (ih * scale).round().clamp(0.0, dh) as i32;
			let x = ((w - draw_w) / 2).max(0);
			let y = ((h - draw_h) / 2).max(0);
			if draw_w > 0 && draw_h > 0
			{
				GdipDrawImageRectI(g, pbitmap, x, y, draw_w, draw_h);
			}
			GdipDeleteGraphics(g);
		}
	}

	EndPaint(hwnd, &ps);
}

unsafe extern "system" fn cover_subclass_proc(hwnd: i64, msg: u32, wparam: usize, lparam: i64) -> i64 {
	if msg == WM_PAINT
	{
		ui_cover_paint(hwnd);
		return 0;
	}
	if msg == WM_ERASEBKGND
	{
		return 1;
	}
	if msg == WM_SIZE
	{
		InvalidateRect(hwnd, null(), 1);
	}
	CallWindowProcW(UI_COVER_OLDPROC, hwnd, msg, wparam, lparam)
}

unsafe fn ui_cover_clear_image() {
	let mut s = UI_COVER_STATE.lock().unwrap();
	if s.pbitmap != 0
	{
		GdipDisposeImage(s.pbitmap);
		s.pbitmap = 0;
		s.img_w = 0;
		s.img_h = 0;
	}

	InvalidateRect(UI_HCOVER, null(), 1);
}

unsafe fn ui_cover_set_image_bytes(bytes: &[u8]) -> bool {
	let pstream = SHCreateMemStream(bytes.as_ptr(), bytes.len().min(u32::MAX as usize) as u32);
	if pstream == 0
	{
		eprintln!("[cover] SHCreateMemStream failed");
		return false;
	}

	let mut pbitmap: i64 = 0;
	let st = GdipCreateBitmapFromStream(pstream, &mut pbitmap);
	ui_cover_com_release(pstream);

	if st != 0 || pbitmap == 0
	{
		eprintln!("[cover] decode failed: status={}", st);
		return false;
	}

	let mut iw: u32 = 0;
	let mut ih: u32 = 0;
	GdipGetImageWidth(pbitmap, &mut iw);
	GdipGetImageHeight(pbitmap, &mut ih);

	{
		let mut s = UI_COVER_STATE.lock().unwrap();
		if s.pbitmap != 0
		{
			GdipDisposeImage(s.pbitmap);
		}
		s.pbitmap = pbitmap;
		s.img_w = iw.max(1);
		s.img_h = ih.max(1);
	}
	//PostMessageW(UI_HWND, 40011, 0, 0);
	InvalidateRect(UI_HCOVER, null(), 1);

	true
}

/// 预留接口：从任意线程通知 UI 加载封面（输入为图片内存）
unsafe fn ui_cover_post_set_image(bytes: Vec<u8>) {
	g_cover_is_show = true;
	let p = Box::into_raw(Box::new(bytes)) as i64;
	if PostMessageW(UI_HWND, WM_UI_COVER_SET, 0, p) == 0
	{
		drop(Box::from_raw(p as *mut Vec<u8>));
	}
}

/// 预留接口：从任意线程通知 UI 清空封面（无封面）
unsafe fn ui_cover_post_clear() {
	g_cover_is_show = false;
	PostMessageW(UI_HWND, WM_UI_COVER_CLEAR, 0, 0);
}

unsafe fn ui_cover_try_load_default() {
	let path = r"D:\cover.jpg";
	if let Ok(bytes) = std::fs::read(path)
	{
		let _ = ui_cover_set_image_bytes(bytes.as_slice());
	}
}

unsafe fn ui_cover_shutdown() {
	ui_cover_clear_image();
	if UI_GDIPLUS_TOKEN != 0
	{
		GdiplusShutdown(UI_GDIPLUS_TOKEN);
		UI_GDIPLUS_TOKEN = 0;
	}
}

fn cover_try_read_embedded_bytes_symphonia(path: &str) -> Option<Vec<u8>> {
	let file = File::open(path).ok()?;
	let mss = MediaSourceStream::new(Box::new(file), Default::default());

	let mut hint = Hint::new();
	if let Some(ext) = Path::new(path)
		.extension()
		.and_then(|s| s.to_str())
	{
		hint.with_extension(ext);
	}

	let meta_opts = MetadataOptions::default().limit_visual_bytes(Limit::Maximum(32 * 1024 * 1024));
	let mut format = symphonia::default::get_probe()
		.probe(&hint, mss, FormatOptions::default(), meta_opts)
		.ok()?;

	let mut best_score: i32 = -1;
	let mut best: Option<Vec<u8>> = None;
	let mut consider = |usage: Option<StandardVisualKey>, data: &[u8]| {
		if data.is_empty()
		{
			return;
		}

		let score = match usage
		{
			Some(StandardVisualKey::FrontCover) => 2,
			Some(_) => 1,
			None => 0,
		};

		if score > best_score
		{
			best_score = score;
			best = Some(data.to_vec());
		}
	};

	{
		let mut meta = format.metadata();
		if let Some(rev) = meta.skip_to_latest()
		{
			for v in rev.media.visuals.iter()
			{
				consider(v.usage, v.data.as_ref());
			}
			for pt in rev.per_track.iter()
			{
				for v in pt.metadata.visuals.iter()
				{
					consider(v.usage, v.data.as_ref());
				}
			}
		}
	}

	best
}

fn cover_has_embedded_symphonia(path: &str) -> bool {
	let Ok(file) = File::open(path)
	else
	{
		return false;
	};
	let mss = MediaSourceStream::new(Box::new(file), Default::default());

	let mut hint = Hint::new();
	if let Some(ext) = Path::new(path)
		.extension()
		.and_then(|s| s.to_str())
	{
		hint.with_extension(ext);
	}

	let meta_opts = MetadataOptions::default().limit_visual_bytes(Limit::Maximum(32 * 1024 * 1024));
	let Ok(mut format) = symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), meta_opts)
	else
	{
		return false;
	};

	{
		let mut meta = format.metadata();
		if let Some(rev) = meta.skip_to_latest()
		{
			if !rev.media.visuals.is_empty()
			{
				return true;
			}
			for pt in rev.per_track.iter()
			{
				if !pt.metadata.visuals.is_empty()
				{
					return true;
				}
			}
		}
	}
	false
}

unsafe fn cover_probe_has_any(info: &SongInfo) -> bool {
	if info.has_cover
	{
		return true;
	}
	if cover_find_sidecar_path(info).is_some()
	{
		return true;
	}
	cover_has_embedded_symphonia(&info.path)
}

/// 播放事件：异步加载封面并更新 UI（输入为音频文件路径）
unsafe fn ui_cover_on_track_change(info: &SongInfo) {
	let track_path = info.path.trim();

	let key = normalize_path_key(track_path);
	{
		let mut last = g_cover_last_path.lock().unwrap();
		if *last == key
		{
			return;
		}
		*last = key;
	}

	let seq = UI_COVER_REQ_SEQ
		.fetch_add(1, Ordering::SeqCst)
		.wrapping_add(1);

	// 先清空，避免上一首封面残留
	ui_cover_post_clear();

	let mut info = info.clone();
	info.duration_ms = seq;

	pool_fn(pl_load_cover, pl_em::th_info(info));
}

unsafe fn pl_load_cover(pl: pl_em) {
	let pl_em::th_info(th) = pl
	else
	{
		return;
	};

	let bytes = cover_load_bytes_for_track(&th);

	// 避免并发切歌导致旧请求覆盖新请求
	if UI_COVER_REQ_SEQ.load(Ordering::SeqCst) != th.duration_ms
	{
		return;
	}

	if let Some(bytes) = bytes
		&& !bytes.is_empty()
	{
		ui_cover_post_set_image(bytes);
	}
	else
	{
		ui_cover_post_clear();
	}
}

unsafe fn cover_find_sidecar_path(info: &SongInfo) -> Option<String> {
	let track_path = info.path.trim();
	let path_fixed: std::borrow::Cow<str> = if track_path.contains('/')
	{
		std::borrow::Cow::Owned(track_path.replace('/', "\\"))
	}
	else
	{
		std::borrow::Cow::Borrowed(track_path)
	};
	let (dir, _) = split_path(&path_fixed);
	if dir.is_empty()
	{
		return None;
	}

	// Reusable buffer: dir + '\\' + max candidate name (folder.jpg = 10)
	let mut buf = String::with_capacity(dir.len() + 1 + 32);
	buf.push_str(dir);
	buf.push('\\');
	let base_len = buf.len();

	const CANDIDATES: [&str; 4] = ["cover.jpg", "folder.jpg", "cover.png", "folder.png"];
	for name in CANDIDATES
	{
		buf.truncate(base_len);
		buf.push_str(name);
		if is_file(&buf)
		{
			return Some(buf);
		}
	}

	let album = info.album.trim();
	if album.is_empty()
	{
		return None;
	}

	buf.truncate(base_len);
	buf.push_str(album);
	buf.push_str(".jpg");
	if is_file(&buf)
	{
		return Some(buf);
	}

	buf.truncate(base_len);
	buf.push_str(album);
	buf.push_str(".png");
	if is_file(&buf)
	{
		return Some(buf);
	}

	None
}

unsafe fn cover_load_bytes_for_track(info: &SongInfo) -> Option<Vec<u8>> {
	if info.has_cover
		&& let Some(bytes) = cover_try_read_embedded_bytes_symphonia(&info.path)
	{
		return Some(bytes);
	};

	if let Some(p) = cover_find_sidecar_path(info) { std::fs::read(p).ok() } else { None }
}

// src\ui_layout.rs
unsafe fn ui_layout(hwnd: i64) {
	// 获取客户区大小
	let mut rc: RECT = zeroed();
	GetClientRect(hwnd, &mut rc);
	let w = (rc.right - rc.left).max(0);
	let h = (rc.bottom - rc.top).max(0);

	// 顶部工具栏 (foobar 风格): 6 个按钮 + 音量条 + 进度条
	let margin = 6;
	let btn_w = 30;
	let btn_h = 28;
	let gap = 4;
	let y_btn = ((TOOLBAR_HEIGHT - btn_h) / 2).max(0);

	let mut x = margin;

	MoveWindow(UI_HBTN_RESTART, x, y_btn, btn_w, btn_h, 1);
	x += btn_w + gap;
	MoveWindow(UI_HBTN_PREV, x, y_btn, btn_w, btn_h, 1);
	x += btn_w + gap;
	MoveWindow(UI_HBTN_PLAY, x, y_btn, btn_w, btn_h, 1);
	x += btn_w + gap;
	MoveWindow(UI_HBTN_PAUSE, x, y_btn, btn_w, btn_h, 1);
	x += btn_w + gap;
	MoveWindow(UI_HBTN_NEXT, x, y_btn, btn_w, btn_h, 1);
	x += btn_w + gap;
	MoveWindow(UI_HBTN_RANDOM, x, y_btn, btn_w, btn_h, 1);
	x += btn_w + gap + 10;

	let mut vol_w = ((w as f32) * 0.18) as i32;
	vol_w = vol_w.clamp(120, 220);
	if w - x - margin < vol_w + 80
	{
		vol_w = (w - x - margin - 80).max(0);
	}
	MoveWindow(UI_HVOL, x, y_btn, vol_w, btn_h, 1);
	x += vol_w + 10;

	let prog_w = (w - x - margin).max(0);
	let y_prog = ((TOOLBAR_HEIGHT - PROGRESS_HIT_HEIGHT) / 2).max(0);
	MoveWindow(UI_HPROGRESS, x, y_prog, prog_w, PROGRESS_HIT_HEIGHT, 1);

	// 播放区：左右两列（共用高度，按比例分配宽度）
	let split_w = if w >= UI_SPLITTER_THICKNESS { UI_SPLITTER_THICKNESS } else { 0 };
	let avail_w = (w - split_w).max(0);

	let ratio_lr = UI_SPLIT_LR
		.load(Ordering::SeqCst)
		.min(1000);
	let mut left_w = ((avail_w as i64 * ratio_lr as i64) / 1000) as i32;
	let min_left_eff = UI_MIN_LEFT_W.min(avail_w);
	let max_left_eff = (avail_w - UI_MIN_RIGHT_W).max(0);
	if max_left_eff >= min_left_eff
	{
		left_w = left_w.clamp(min_left_eff, max_left_eff);
	}
	else
	{
		left_w = avail_w;
	}
	let right_w = (w - left_w - split_w).max(0);

	let tree_x = left_w + split_w;
	let right_total_h = (h - PROGRESS_TOTAL_HEIGHT).max(0);

	// 右侧：封面 + TreeView（按比例分配高度）
	let split_h2 = if right_total_h >= UI_SPLITTER_THICKNESS { UI_SPLITTER_THICKNESS } else { 0 };
	let avail_h2 = (right_total_h - split_h2).max(0);

	let ratio_cover = UI_SPLIT_COVER_TREE
		.load(Ordering::SeqCst)
		.min(1000);
	let mut cover_h = ((avail_h2 as i64 * ratio_cover as i64) / 1000) as i32;
	let min_cover_eff = UI_MIN_COVER_H.min(avail_h2);
	let max_cover_eff = (avail_h2 - UI_MIN_TREE_H).max(0);
	if max_cover_eff >= min_cover_eff
	{
		cover_h = cover_h.clamp(min_cover_eff, max_cover_eff);
	}
	else
	{
		cover_h = avail_h2;
	}
	let tree_h = (right_total_h - cover_h - split_h2).max(0);

	MoveWindow(UI_HCOVER, tree_x, PROGRESS_TOTAL_HEIGHT, right_w, cover_h, 1);
	MoveWindow(UI_HSPLIT_COVER_TREE, tree_x, PROGRESS_TOTAL_HEIGHT + cover_h, right_w, split_h2, 1);
	MoveWindow(UI_HTREE, tree_x, PROGRESS_TOTAL_HEIGHT + cover_h + split_h2, right_w, tree_h, 1);
	// 左右分隔条
	MoveWindow(UI_HSPLIT_LR, left_w, PROGRESS_TOTAL_HEIGHT, split_w, right_total_h, 1);

	// 左侧列：Tab + (ListView / Log)（共用宽度，按比例分配高度）
	MoveWindow(UI_HTAB, 0, PROGRESS_TOTAL_HEIGHT, left_w, TAB_HEIGHT, 1);

	let top = PROGRESS_TOTAL_HEIGHT + TAB_HEIGHT;
	let total_left_h = (h - top).max(0);

	let split_h = if total_left_h >= UI_SPLITTER_THICKNESS { UI_SPLITTER_THICKNESS } else { 0 };
	let avail_h = (total_left_h - split_h).max(0);

	let ratio_list = UI_SPLIT_LIST_LOG
		.load(Ordering::SeqCst)
		.min(1000);
	let mut list_h = ((avail_h as i64 * ratio_list as i64) / 1000) as i32;
	let min_list_eff = UI_MIN_LIST_H.min(avail_h);
	let max_list_eff = (avail_h - UI_MIN_LOG_H).max(0);
	if max_list_eff >= min_list_eff
	{
		list_h = list_h.clamp(min_list_eff, max_list_eff);
	}
	else
	{
		list_h = avail_h;
	}
	let log_h = (total_left_h - list_h - split_h).max(0);

	for v in ui_pl_views_li.iter_mut()
	{
		MoveWindow(v.hlist, 0, top, left_w, list_h, 1);
		if v.hlist == 0
		{
			continue;
		}

		// 初次初始化：按比例铺满（避免隐藏 tab 的反复重算）。
		if !v.cols_inited
		{
			if ui_apply_playlist_column_ratios(v.hlist)
			{
				v.cols_inited = true;
				let mut rc: RECT = zeroed();
				if GetClientRect(v.hlist, &mut rc) != 0
				{
					v.cols_last_total_w = (rc.right - rc.left).max(0);
				}
			}
			continue;
		}

		// 仅对当前显示的 ListView 在宽度变化时重算列宽，避免隐藏 tab 的 N 倍开销。
		if v.hlist != UI_HLIST
		{
			continue;
		}
		let mut rc: RECT = zeroed();
		if GetClientRect(v.hlist, &mut rc) == 0
		{
			continue;
		}
		let total_w = (rc.right - rc.left).max(0);
		if total_w > 0 && v.cols_last_total_w != total_w
		{
			if ui_apply_playlist_column_ratios(v.hlist)
			{
				v.cols_last_total_w = total_w;
			}
		}
	}
	MoveWindow(UI_HSPLIT_LIST_LOG, 0, top + list_h, left_w, split_h, 1);
	MoveWindow(UI_HLOG, 0, top + list_h + split_h, left_w, log_h, 1);
}

unsafe fn ui_apply_split_drag(hwnd: i64, x: i32, y: i32) {
	// 获取客户区大小
	let mut rc: RECT = zeroed();
	GetClientRect(hwnd, &mut rc);
	let w = (rc.right - rc.left).max(0);
	let h = (rc.bottom - rc.top).max(0);

	match UI_DRAG_MODE
	{
		1 =>
		{
			// 左右分隔条拖动：更新左列宽度比例
			let split_w = if w >= UI_SPLITTER_THICKNESS { UI_SPLITTER_THICKNESS } else { 0 };
			let avail_w = (w - split_w).max(0);
			if avail_w > 0
			{
				let mut left_w = x.clamp(0, avail_w);
				let min_left_eff = UI_MIN_LEFT_W.min(avail_w);
				let max_left_eff = (avail_w - UI_MIN_RIGHT_W).max(0);
				if max_left_eff >= min_left_eff
				{
					left_w = left_w.clamp(min_left_eff, max_left_eff);
				}
				else
				{
					left_w = avail_w;
				}

				let ratio = ((left_w as i64 * 1000) / (avail_w as i64)) as u32;
				UI_SPLIT_LR.store(ratio.min(1000), Ordering::SeqCst);
			}
		}
		2 =>
		{
			// 列表/日志分隔条拖动：更新列表高度比例
			let top = PROGRESS_TOTAL_HEIGHT + TAB_HEIGHT;
			let total_left_h = (h - top).max(0);
			let split_h = if total_left_h >= UI_SPLITTER_THICKNESS { UI_SPLITTER_THICKNESS } else { 0 };
			let avail_h = (total_left_h - split_h).max(0);
			if avail_h > 0
			{
				let mut list_h = (y - top).clamp(0, avail_h);
				let min_list_eff = UI_MIN_LIST_H.min(avail_h);
				let max_list_eff = (avail_h - UI_MIN_LOG_H).max(0);
				if max_list_eff >= min_list_eff
				{
					list_h = list_h.clamp(min_list_eff, max_list_eff);
				}
				else
				{
					list_h = avail_h;
				}

				let ratio = ((list_h as i64 * 1000) / (avail_h as i64)) as u32;
				UI_SPLIT_LIST_LOG.store(ratio.min(1000), Ordering::SeqCst);
			}
		}
		3 =>
		{
			// 封面/Tree 分隔条拖动：更新封面高度比例
			let top = PROGRESS_TOTAL_HEIGHT;
			let total_right_h = (h - top).max(0);
			let split_h = if total_right_h >= UI_SPLITTER_THICKNESS { UI_SPLITTER_THICKNESS } else { 0 };
			let avail_h = (total_right_h - split_h).max(0);
			if avail_h > 0
			{
				let mut cover_h = (y - top).clamp(0, avail_h);
				let min_cover_eff = UI_MIN_COVER_H.min(avail_h);
				let max_cover_eff = (avail_h - UI_MIN_TREE_H).max(0);
				if max_cover_eff >= min_cover_eff
				{
					cover_h = cover_h.clamp(min_cover_eff, max_cover_eff);
				}
				else
				{
					cover_h = avail_h;
				}

				let ratio = ((cover_h as i64 * 1000) / (avail_h as i64)) as u32;
				UI_SPLIT_COVER_TREE.store(ratio.min(1000), Ordering::SeqCst);
			}
		}
		_ =>
		{}
	}

	ui_layout(hwnd);
}

// src\ui_msg.rs
unsafe extern "system" fn ui_window_proc(hwnd: i64, msg: u32, wparam: usize, lparam: i64) -> i64 {
	match msg
	{
		WM_CREATE =>
		{
			ui_msg_create(hwnd, wparam, lparam) // -> ui_msg_create.rs
		}
		WM_SIZE =>
		{
			ui_layout(hwnd);

			// 触发延迟保存
			SetTimer(hwnd, ID_SAVE_TIMER, 500, 0);
			0
		}
		WM_MOVE =>
		{
			// 触发延迟保存
			SetTimer(hwnd, ID_SAVE_TIMER, 500, 0);
			0
		}
		WM_TIMER =>
		{
			if wparam == ID_SAVE_TIMER
			{
				KillTimer(hwnd, ID_SAVE_TIMER);
				ui_save_window_state();
			}
			0
		}
		WM_CLOSE =>
		{
			// Save state before hiding so "close" doesn't persist a hidden start state.
			ui_save_window_state();
			ShowWindow(hwnd, 0); // SW_HIDE
			g_ui_is_visible = false;
			PostMessageW(G_HWND, WM_CLOSE, 0, 0);
			0
		}
		WM_DESTROY =>
		{
			ui_destroy();
			0
		}
		WM_UI_PLAYLIST_UPDATE =>
		{
			let li_id = wparam;
			if lparam != 0
			{
				let boxed = Box::from_raw(lparam as *mut Vec<SongInfo>);
				let mut hlist = ui_listview_for_li_id(li_id);
				if hlist == 0
				{
					if let Some(idx) = ui_ensure_playlist_view(li_id)
					{
						hlist = ui_pl_views_li[idx].hlist;
					}
				}
				if hlist != 0
				{
					do_playlist_update(hlist, &boxed);
				}
			}
			0
		}
		WM_UI_PLAYLIST_SELECT =>
		{
			let li_id = wparam;
			let idx = lparam as i32;
			let hlist = ui_listview_for_li_id(li_id);
			if hlist == 0
			{
				return 0;
			}
			// 先取消所有选中
			let mut lvi: LVITEMW = zeroed();
			lvi.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
			lvi.state = 0;
			SendMessageW(hlist, LVM_SETITEMSTATE, usize::MAX, &lvi as *const _ as i64);
			// 选中指定项
			lvi.state = LVIS_SELECTED | LVIS_FOCUSED;
			SendMessageW(hlist, LVM_SETITEMSTATE, idx as usize, &lvi as *const _ as i64);
			// 确保可见
			const LVM_ENSUREVISIBLE: u32 = LVM_FIRST + 19;
			SendMessageW(hlist, LVM_ENSUREVISIBLE, idx as usize, 0);
			0
		}
		WM_UI_TAB_SYNC =>
		{
			let li_id = wparam;
			if let Some(idx) = ui_ensure_playlist_view(li_id)
			{
				SendMessageW(UI_HTAB, TCM_SETCURSEL, idx as usize, 0);
				ui_apply_tab_selection(idx as i32);
			}
			0
		}
		WM_SEEK_FWD =>
		{
			let delta = if lparam == 0 { 5 } else { (lparam as i32).min(100) };
			g_to_seek.fetch_add(delta, Ordering::SeqCst);
			0
		}
		WM_SEEK_BWD =>
		{
			let delta = if lparam == 0 { 5 } else { (lparam as i32).min(100) };
			g_to_seek.fetch_sub(delta, Ordering::SeqCst);
			0
		}
		WM_UI_NOW_PLAYING =>
		{
			let li_id = wparam;

			let idx = lparam as _;

			let old_li_id = UI_CURRENT_PLAYING_LI_ID;
			let old_idx = UI_CURRENT_PLAYING_IDX;

			// 更新当前播放状态（必须在重绘之前设置，以便回调能获取正确的标记）
			UI_CURRENT_PLAYING_LI_ID = li_id;
			UI_CURRENT_PLAYING_IDX = idx;

			// 重绘旧的播放项（移除 ▶ 标记）
			if old_idx >= 0 && (old_li_id != li_id || old_idx != idx)
			{
				let old_hlist = ui_listview_for_li_id(old_li_id);
				if old_hlist != 0
				{
					// 使用 LVM_REDRAWITEMS 触发虚拟列表重绘
					SendMessageW(old_hlist, LVM_REDRAWITEMS, old_idx as usize, old_idx as i64);
				}
			}

			// 重绘新的播放项（添加 ▶ 标记）
			if idx >= 0
			{
				let mut hlist = ui_listview_for_li_id(li_id);
				if hlist == 0
				{
					if let Some(idx_view) = ui_ensure_playlist_view(li_id)
					{
						hlist = ui_pl_views_li[idx_view].hlist;
					}
				}
				if hlist != 0
				{
					// 使用 LVM_REDRAWITEMS 触发虚拟列表重绘
					SendMessageW(hlist, LVM_REDRAWITEMS, idx as usize, idx as i64);
				}
			}

			0
		}
		WM_DRAWITEM =>
		{
			let id = wparam as i64;
			if (id == ID_SPLIT_LR || id == ID_SPLIT_LIST_LOG || id == ID_SPLIT_COVER_TREE) && lparam != 0
			{
				let dis = lparam as *const DRAWITEMSTRUCT;
				let hbr = UI_HSPLIT_BRUSH;
				if hbr != 0
				{
					FillRect((*dis).hDC, &(*dis).rcItem as *const RECT, hbr);
				}
				return 1;
			}
			0
		}
		WM_MOUSEMOVE =>
		{
			if UI_DRAG_MODE != 0
			{
				let x = (lparam & 0xFFFF) as i16 as i32;
				let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
				ui_apply_split_drag(hwnd, x, y);
				if UI_DRAG_MODE == 1
				{
					SetCursor(LoadCursorW(0, IDC_SIZEWE));
				}
				else if UI_DRAG_MODE == 2 || UI_DRAG_MODE == 3
				{
					SetCursor(LoadCursorW(0, IDC_SIZENS));
				}
				return 0;
			}
			DefWindowProcW(hwnd, msg, wparam, lparam)
		}
		WM_SETCURSOR =>
		{
			if UI_DRAG_MODE == 1
			{
				SetCursor(LoadCursorW(0, IDC_SIZEWE));
				return 1;
			}
			if UI_DRAG_MODE == 2 || UI_DRAG_MODE == 3
			{
				SetCursor(LoadCursorW(0, IDC_SIZENS));
				return 1;
			}
			DefWindowProcW(hwnd, msg, wparam, lparam)
		}
		WM_LBUTTONUP =>
		{
			if UI_DRAG_MODE != 0
			{
				UI_DRAG_MODE = 0;
				ReleaseCapture();
				ui_save_window_state();
				return 0;
			}
			DefWindowProcW(hwnd, msg, wparam, lparam)
		}
		WM_COMMAND =>
		{
			let id = (wparam & 0xFFFF) as i64;
			let handled = match id
			{
				ID_BTN_RESTART =>
				{
					PostMessageW(G_HWND, WM_RESTART, 0, 0);
					true
				}
				ID_BTN_PREV =>
				{
					PostMessageW(G_HWND, WM_PREV_TRACK, 0, 0);
					true
				}
				ID_BTN_PLAY =>
				{
					PostMessageW(G_HWND, WM_RESUME, 0, 0);
					true
				}
				ID_BTN_PAUSE =>
				{
					PostMessageW(G_HWND, WM_PAUSE, 0, 0);
					true
				}
				ID_BTN_NEXT =>
				{
					PostMessageW(G_HWND, WM_NEXT_TRACK, 0, 0);
					true
				}
				ID_BTN_RANDOM =>
				{
					PostMessageW(G_HWND, WM_RANDOM_NEXT_TRACK, 0, 0);
					true
				}
				_ => false,
			};

			// 让播放列表保持焦点，避免高亮丢失
			if handled
			{
				SetFocus(UI_HLIST);
				return 0;
			}
			DefWindowProcW(hwnd, msg, wparam, lparam)
		}
		WM_HSCROLL =>
		{
			if lparam == UI_HVOL && UI_HVOL != 0
			{
				let code = (wparam & 0xFFFF) as u32;
				let pos = SendMessageW(UI_HVOL, TBM_GETPOS, 0, 0) as i32;
				let pos = pos.clamp(0, 100) as u32;
				let prev = g_to_volume.swap(pos, Ordering::SeqCst);
				if code == TB_ENDTRACK && prev != pos
				{
					db_save_volume(pos);
				}
				return 0;
			}
			DefWindowProcW(hwnd, msg, wparam, lparam)
		}
		WM_UI_VOLUME_SYNC =>
		{
			let pos = (wparam as i32).clamp(0, 100);
			if UI_HVOL != 0
			{
				SendMessageW(UI_HVOL, TBM_SETPOS, 1, pos as i64);
			}
			0
		}
		WM_UI_PLAY_STATE =>
		{
			let is_playing = wparam != 0;
			if UI_TOOLBAR_PLAYING != is_playing
			{
				UI_TOOLBAR_PLAYING = is_playing;
				if UI_HPROGRESS != 0
				{
					InvalidateRect(UI_HPROGRESS, null(), 1);
				}
			}
			0
		}
		WM_UI_LOG_FLUSH =>
		{
			ui_log_flush();
			0
		}
		WM_UI_COVER_SET =>
		{
			if lparam != 0
			{
				let boxed = Box::from_raw(lparam as *mut Vec<u8>);
				let _ = ui_cover_set_image_bytes(boxed.as_slice());
			}
			0
		}
		WM_UI_COVER_CLEAR =>
		{
			ui_cover_clear_image();
			0
		}
		WM_NOTIFY => ui_notify(wparam, lparam),
		// WM_EXITSIZEMOVE
		0x0232 =>
		{
			ui_save_window_state();
			0
		}
		40000 =>
		{
			ui_tab_id_to_del(wparam);
			0
		}
		40001 =>
		{
			ui_tab_id_to_pl(wparam);
			0
		}
		40003 =>
		{
			if lparam < 0
			{
				return 0;
			}
			pl_del_item(wparam, vec![lparam as usize]);
			0
		}

		40004 =>
		{
			if lparam >= 0
			{
				ui_pl_id_to_play(wparam, lparam as _);
			};
			0
		}
		40005 =>
		{
			if lparam != 0
			{
				ui_tree_id_to_pl(lparam);
			};
			0
		}
		WM_COPYDATA => ui_msg_copy_data(wparam, lparam),
		40007 => ui_pl_sort(wparam, lparam),
		WM_TOGGLE_WINDOW =>
		{
			ui_toggle_visibility();
			0
		}
		// 40006
		WM_UI_TREE_REFRESH =>
		{
			ui_music_tree_rebuild();
			eprintln!("[ui] 重置 音乐树");
			0
		}
		40008 =>
		{
			let sel = SendMessageW(UI_HTAB, TCM_GETCURSEL, 0, 0);
			if sel >= 0 { ui_pl_views_li[sel as usize].playlist_id as _ } else { 0 }
		}
		40009 =>
		{
			ui_pl_reset_from_id(wparam);
			0
		}
		40010 =>
		{
			ui_wm_play_pause();
			0
		}
		40011 =>
		{
			InvalidateRect(UI_HCOVER, null(), 1);
			0
		}
		WM_UI_PLAY_MODE_SINGLE =>
		{
			let li_id = g_li_id.load(Ordering::SeqCst);
			if li_id != 0 && g_pl_mode.load(Ordering::SeqCst) != 0
			{
				g_pl_mode.store(0, Ordering::SeqCst);
				db_save_playlist_play_mode(li_id, 0);
				db_save_play_mode(0);
			}
			eprintln!("[ui] 切换到 单曲循环 模式");
			0
		}
		_ => DefWindowProcW(hwnd, msg, wparam, lparam),
	}
}

// src\ui_msg_copy_data.rs
unsafe fn ui_msg_copy_data(wparam: usize, lparam: i64) -> i64 {
	#[repr(C)]
	struct COPYDATASTRUCT {
		dwData: usize,
		cbData: u32,
		lpData: *const u8,
	}

	let cds = lparam as *const COPYDATASTRUCT;
	if cds.is_null()
	{
		return 0;
	}

	let cb = (*cds).cbData as usize;
	let p = (*cds).lpData;
	if cb == 0 || p.is_null()
	{
		return 1;
	}

	let bytes = from_raw_parts(p, cb);

	// Accept UTF-8 (optional) and UTF-16LE (AutoHotkey default).
	let mut s_utf8 = String::from_utf8_lossy(bytes).into_owned();

	while s_utf8.ends_with('\0')
	{
		s_utf8.pop();
	}

	let looks_like_utf16le = cb >= 4 && cb % 2 == 0 && s_utf8.contains('\0');
	let s = if looks_like_utf16le
	{
		let mut u16s: Vec<u16> = Vec::with_capacity(bytes.len() / 2);
		for c in bytes.chunks_exact(2)
		{
			u16s.push(u16::from_le_bytes([c[0], c[1]]));
		}
		while u16s.last() == Some(&0)
		{
			u16s.pop();
		}
		String::from_utf16_lossy(&u16s)
	}
	else
	{
		s_utf8
	};

	let s = s.trim().to_string();
	if let Some((cmd, st)) = s.split_once('|')
	{
		let cmd = cmd.trim();
		let st = st.trim();
		match cmd
		{
			"path_del" =>
			{
				if st.is_empty()
				{
					eprintln!("[copydata] path_del: empty path");
					return 1;
				}

				let path = if st.contains('/') { st.replace('/', "\\") } else { st.to_string() };

				// SQL: collect (playlist_id, idx) by path, then group as map<pl_id, [idx]>
				let mut map: std::collections::BTreeMap<usize, Vec<usize>> = std::collections::BTreeMap::new();
				for (pl_id, idxs) in db_collect_playlist_delete_plan_by_path(&path)
				{
					map.entry(pl_id)
						.or_default()
						.extend(idxs);
				}

				if map.is_empty()
				{
					eprintln!("[copydata] path_del: not found: {}", path);
					return 1;
				}

				for (pl_id, idxs) in map
				{
					pl_del_item(pl_id, idxs);
				}

				eprintln!("[copydata] path_del: {}", path);
			}
			_ =>
			{
				eprintln!("[copydata] {}", s);
			}
		};
	}
	else
	{
		eprintln!("[copydata] {}", s);
	}
	1
}

// src\ui_msg_create.rs
static mut g_font_size: i32 = 20;

unsafe fn ui_msg_create(hwnd: i64, wparam: usize, lparam: i64) -> i64 {
	UI_HWND = hwnd;

	// 初始化 Common Controls
	let icce = INITCOMMONCONTROLSEX {
		dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
		dwICC: ICC_PROGRESS_CLASS | ICC_LISTVIEW_CLASSES | ICC_TAB_CLASSES | ICC_TREEVIEW_CLASSES | ICC_BAR_CLASSES,
	};
	InitCommonControlsEx(&icce);

	// 创建字体
	let font_name = to_wstring("Consolas");
	UI_HFONT = CreateFontW(g_font_size, 0, 0, 0, 400, 0, 0, 0, 0, 0, 0, 5, 0, font_name.as_ptr());
	let icon_font_name = to_wstring("Segoe UI Symbol");
	UI_HFONT_ICON = CreateFontW(18, 0, 0, 0, 400, 0, 0, 0, 0, 0, 0, 5, 0, icon_font_name.as_ptr());
	if UI_HFONT_ICON == 0
	{
		UI_HFONT_ICON = UI_HFONT;
	}

	// 创建工具栏按钮 + 音量条 + 进度条 (顶部)
	// 先创建状态指示背景画刷
	UI_HBRUSH_PROGRESS_PLAY_BG = CreateSolidBrush(UI_PROGRESS_PLAY_BK_COLOR);
	UI_HBRUSH_PROGRESS_PAUSE_BG = CreateSolidBrush(UI_PROGRESS_PAUSE_BK_COLOR);
	UI_HBRUSH_PROGRESS_PLAY_BAR = CreateSolidBrush(UI_PROGRESS_PLAY_BAR_COLOR);
	UI_HBRUSH_PROGRESS_PAUSE_BAR = CreateSolidBrush(UI_PROGRESS_PAUSE_BAR_COLOR);

	let button_class = to_wstring("BUTTON");
	UI_HBTN_RESTART = CreateWindowExW(
		0,
		button_class.as_ptr(),
		to_wstring("⟲").as_ptr(),
		WS_CHILD | WS_VISIBLE | BS_FLAT,
		0,
		0,
		24,
		24,
		hwnd,
		ID_BTN_RESTART,
		0,
		0,
	);
	UI_HBTN_PREV = CreateWindowExW(
		0,
		button_class.as_ptr(),
		to_wstring("⏮").as_ptr(),
		WS_CHILD | WS_VISIBLE | BS_FLAT,
		0,
		0,
		24,
		24,
		hwnd,
		ID_BTN_PREV,
		0,
		0,
	);
	UI_HBTN_PLAY = CreateWindowExW(
		0,
		button_class.as_ptr(),
		to_wstring("▶").as_ptr(),
		WS_CHILD | WS_VISIBLE | BS_FLAT,
		0,
		0,
		24,
		24,
		hwnd,
		ID_BTN_PLAY,
		0,
		0,
	);
	UI_HBTN_PAUSE = CreateWindowExW(
		0,
		button_class.as_ptr(),
		to_wstring("⏸").as_ptr(),
		WS_CHILD | WS_VISIBLE | BS_FLAT,
		0,
		0,
		24,
		24,
		hwnd,
		ID_BTN_PAUSE,
		0,
		0,
	);
	UI_HBTN_NEXT = CreateWindowExW(
		0,
		button_class.as_ptr(),
		to_wstring("⏭").as_ptr(),
		WS_CHILD | WS_VISIBLE | BS_FLAT,
		0,
		0,
		24,
		24,
		hwnd,
		ID_BTN_NEXT,
		0,
		0,
	);
	UI_HBTN_RANDOM = CreateWindowExW(
		0,
		button_class.as_ptr(),
		to_wstring("🔀").as_ptr(),
		WS_CHILD | WS_VISIBLE | BS_FLAT,
		0,
		0,
		24,
		24,
		hwnd,
		ID_BTN_RANDOM,
		0,
		0,
	);

	SendMessageW(UI_HBTN_RESTART, WM_SETFONT, UI_HFONT_ICON as usize, 1);
	SendMessageW(UI_HBTN_PREV, WM_SETFONT, UI_HFONT_ICON as usize, 1);
	SendMessageW(UI_HBTN_PLAY, WM_SETFONT, UI_HFONT_ICON as usize, 1);
	SendMessageW(UI_HBTN_PAUSE, WM_SETFONT, UI_HFONT_ICON as usize, 1);
	SendMessageW(UI_HBTN_NEXT, WM_SETFONT, UI_HFONT_ICON as usize, 1);
	SendMessageW(UI_HBTN_RANDOM, WM_SETFONT, UI_HFONT_ICON as usize, 1);

	let trackbar_class = to_wstring("msctls_trackbar32");
	UI_HVOL =
		CreateWindowExW(0, trackbar_class.as_ptr(), null_mut(), WS_CHILD | WS_VISIBLE | TBS_NOTICKS, 0, 0, 160, 24, hwnd, ID_VOLUME, 0, 0);
	// 范围 0-100
	SendMessageW(UI_HVOL, TBM_SETRANGE, 1, ((100i64) << 16) | 0);
	SendMessageW(UI_HVOL, TBM_SETPOS, 1, g_to_volume.load(Ordering::SeqCst) as i64);

	UI_HPROGRESS = CreateWindowExW(
		0,
		PROGRESS_CLASS.as_ptr(),
		null_mut(),
		WS_CHILD | WS_VISIBLE | PBS_SMOOTH,
		0,
		0,
		0,
		PROGRESS_HIT_HEIGHT,
		hwnd,
		ID_PROGRESS,
		0,
		0,
	);
	// 设置进度条范围为 0-1000
	SendMessageW(UI_HPROGRESS, PBM_SETRANGE32, 0, 1000);
	// 子类化进度条以处理点击事件
	UI_PROGRESS_OLDPROC = SetWindowLongPtrW(UI_HPROGRESS, GWLP_WNDPROC, progress_subclass_proc as i64);
	if UI_HPROGRESS != 0
	{
		InvalidateRect(UI_HPROGRESS, null(), 1);
	}

	// 创建 TabControl (播放列表选项卡)
	let tab_class = to_wstring("SysTabControl32");
	UI_HTAB = CreateWindowExW(
		0,
		tab_class.as_ptr(),
		null_mut(),
		WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
		0,
		PROGRESS_TOTAL_HEIGHT,
		0,
		TAB_HEIGHT,
		hwnd,
		ID_TAB,
		0,
		0,
	);

	SendMessageW(UI_HTAB, WM_SETFONT, UI_HFONT as usize, 1);
	UI_TAB_OLDPROC = SetWindowLongPtrW(UI_HTAB, GWLP_WNDPROC, tab_subclass_proc as i64);

	ui_pl_views_li.clear();
	let listview_class = to_wstring("SysListView32");
	let playlists = db_load_playlists();
	for (i, (pl_id, name)) in playlists.iter().enumerate()
	{
		let display_name = if name.trim().is_empty() { format!("playlist {}", pl_id) } else { name.clone() };
		let text = to_wstring(&display_name);
		let mut item: TCITEMW = zeroed();
		item.mask = TCIF_TEXT;
		item.pszText = text.as_ptr();
		SendMessageW(UI_HTAB, TCM_INSERTITEMW, i, &item as *const _ as i64);

		let hlist = CreateWindowExW(
			0,
			listview_class.as_ptr(),
			null_mut(),
			WS_CHILD | WS_VSCROLL | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_OWNERDATA,
			0,
			PROGRESS_TOTAL_HEIGHT + TAB_HEIGHT,
			0,
			0,
			hwnd,
			ID_LISTVIEW_BASE + i as i64,
			0,
			0,
		);
		if hlist != 0
		{
			ui_init_playlist_listview(hlist);
		}
		ui_pl_views_li.push(UiPlaylistView {
			playlist_id: *pl_id,
			name: display_name,
			hlist,
			hheader: SendMessageW(hlist, LVM_GETHEADER, 0, 0),
			cols_inited: false,
			cols_last_total_w: 0,
			sort_col: -1,
			sort_asc: true,
		});
	}

	SendMessageW(UI_HTAB, TCM_SETCURSEL, 0, 0);
	ui_apply_tab_selection(0);

	if let Ok(pool) = m_pl_pool.read()
	{
		for v in ui_pl_views_li.iter()
		{
			if let Some(li) = pool.get(&v.playlist_id)
			{
				do_playlist_update(v.hlist, li);
			}
		}
	}

	let edit_class = to_wstring("EDIT");
	UI_HLOG = CreateWindowExW(
		0,
		edit_class.as_ptr(),
		null_mut(),
		WS_CHILD | WS_VISIBLE | WS_VSCROLL | ES_LEFT | ES_MULTILINE | ES_AUTOVSCROLL | ES_READONLY,
		0,
		200 + PROGRESS_TOTAL_HEIGHT + TAB_HEIGHT,
		0,
		0,
		hwnd,
		ID_LOG_EDIT,
		0,
		0,
	);

	SendMessageW(UI_HLOG, WM_SETFONT, UI_HFONT as usize, 1);
	// EDIT 控件默认有 64K 左右的文本长度上限，达到后会出现“追加内容被截断 + 后续不再追加”的症状。
	// 这里将上限放大到最大值，并通过行数裁剪来限制实际占用。
	SendMessageW(UI_HLOG, EM_SETLIMITTEXT, UI_LOG_EDIT_LIMIT_CHARS, 0);

	ui_log_flush();

	// Tree 上方封面控件（默认空白，支持内存图片渲染）
	let static_class = to_wstring("STATIC");
	UI_HCOVER =
		CreateWindowExW(0, static_class.as_ptr(), null_mut(), WS_CHILD | WS_VISIBLE, 0, PROGRESS_TOTAL_HEIGHT, 0, 0, hwnd, ID_COVER, 0, 0);
	UI_COVER_OLDPROC = SetWindowLongPtrW(UI_HCOVER, GWLP_WNDPROC, cover_subclass_proc as i64);

	// 创建 TreeView (音乐库)
	let tree_class = to_wstring("SysTreeView32");
	UI_HTREE = CreateWindowExW(
		0,
		tree_class.as_ptr(),
		null_mut(),
		WS_CHILD | WS_VISIBLE | TVS_HASBUTTONS | TVS_HASLINES | TVS_LINESATROOT | TVS_SHOWSELALWAYS,
		0,
		PROGRESS_TOTAL_HEIGHT,
		0,
		0,
		hwnd,
		ID_TREEVIEW,
		0,
		0,
	);
	SendMessageW(UI_HTREE, WM_SETFONT, UI_HFONT as usize, 1);
	// Reduce flicker during lazy insertions / expansion.
	SendMessageW(UI_HTREE, TVM_SETEXTENDEDSTYLE, TVS_EX_DOUBLEBUFFER as usize, TVS_EX_DOUBLEBUFFER as i64);
	UI_TREE_OLDPROC = SetWindowLongPtrW(UI_HTREE, GWLP_WNDPROC, tree_subclass_proc as i64);

	// 创建分隔条：左右竖列 / 列表-日志
	UI_HSPLIT_LR = CreateWindowExW(
		0,
		static_class.as_ptr(),
		null_mut(),
		WS_CHILD | WS_VISIBLE | SS_NOTIFY | SS_OWNERDRAW,
		0,
		PROGRESS_TOTAL_HEIGHT,
		UI_SPLITTER_THICKNESS,
		0,
		hwnd,
		ID_SPLIT_LR,
		0,
		0,
	);
	UI_HSPLIT_LIST_LOG = CreateWindowExW(
		0,
		static_class.as_ptr(),
		null_mut(),
		WS_CHILD | WS_VISIBLE | SS_NOTIFY | SS_OWNERDRAW,
		0,
		PROGRESS_TOTAL_HEIGHT + TAB_HEIGHT,
		0,
		UI_SPLITTER_THICKNESS,
		hwnd,
		ID_SPLIT_LIST_LOG,
		0,
		0,
	);
	UI_HSPLIT_COVER_TREE = CreateWindowExW(
		0,
		static_class.as_ptr(),
		null_mut(),
		WS_CHILD | WS_VISIBLE | SS_NOTIFY | SS_OWNERDRAW,
		0,
		PROGRESS_TOTAL_HEIGHT,
		0,
		UI_SPLITTER_THICKNESS,
		hwnd,
		ID_SPLIT_COVER_TREE,
		0,
		0,
	);
	if UI_HSPLIT_BRUSH == 0
	{
		UI_HSPLIT_BRUSH = CreateSolidBrush(UI_SPLITTER_COLOR);
	}
	UI_SPLIT_LR_OLDPROC = SetWindowLongPtrW(UI_HSPLIT_LR, GWLP_WNDPROC, splitter_lr_subclass_proc as i64);
	UI_SPLIT_LIST_LOG_OLDPROC = SetWindowLongPtrW(UI_HSPLIT_LIST_LOG, GWLP_WNDPROC, splitter_list_log_subclass_proc as i64);
	UI_SPLIT_COVER_TREE_OLDPROC = SetWindowLongPtrW(UI_HSPLIT_COVER_TREE, GWLP_WNDPROC, splitter_cover_tree_subclass_proc as i64);

	let bad_list = ui_pl_views_li.is_empty()
		|| ui_pl_views_li
			.iter()
			.any(|v| v.hlist == 0);
	if UI_HPROGRESS == 0
		|| UI_HVOL == 0
		|| UI_HBTN_RESTART == 0
		|| UI_HBTN_PREV == 0
		|| UI_HBTN_PLAY == 0
		|| UI_HBTN_PAUSE == 0
		|| UI_HBTN_NEXT == 0
		|| UI_HBTN_RANDOM == 0
		|| bad_list
		|| UI_HTAB == 0
		|| UI_HLOG == 0
		|| UI_HCOVER == 0
		|| UI_HTREE == 0
		|| UI_HSPLIT_LR == 0
		|| UI_HSPLIT_LIST_LOG == 0
		|| UI_HSPLIT_COVER_TREE == 0
	{
		eprintln!("错误: 创建 子控件 失败.\n程序将退出");
		msg_box("错误: 创建 子控件 失败.\n程序将退出", "错误", MB_ICONERROR | MB_OK);
		PostMessageW(G_HWND, WM_DESTROY, 0, 0);
		return 0;
	}

	0
}

// src\ui_msg_to.rs
unsafe fn ui_wm_play_pause() {
	let state = get_player_state();
	if state != PlayerState::Playing && state != PlayerState::Paused
	{
		return;
	}

	// 用事件状态判断是否已暂停：WAIT_TIMEOUT(258) = 已暂停, WAIT_OBJECT_0(0) = 播放中
	let was_paused = WaitForSingleObject(g_ev_resume, 0) == 258;

	if was_paused
	{
		// 恢复：设置恢复事件（唤醒等待线程）
		SetEvent(g_ev_resume);
		db_save_playing_status(true);
	}
	else
	{
		// 暂停：重置恢复事件（阻塞等待线程）
		ResetEvent(g_ev_resume);
		db_save_playing_status(false);
	}

	if g_is_exclusive.load(Ordering::SeqCst)
	{
		// 暂停=释放Exclusive(发1), 恢复=请求Exclusive(发0)
		// HWND_BROADCAST = 0xFFFF
		PostMessageW(0xFFFF, 51000, if !was_paused { 1 } else { 0 }, G_HWND);
	}
}

// src\ui_notify.rs
unsafe fn ui_notify(wparam: usize, lparam: i64) -> i64 {
	let nmhdr = lparam as *const NMHDR;
	let code = (*nmhdr).code;
	let id_from = (*nmhdr).idFrom;
	if id_from == ID_TAB as usize && code == TCN_SELCHANGE
	{
		let sel = SendMessageW(UI_HTAB, TCM_GETCURSEL, 0, 0) as i32;
		ui_apply_tab_selection(sel);
	}
	else if id_from == ID_TREEVIEW as usize && code == TVN_ITEMEXPANDINGW
	{
		let tv = lparam as *const NMTREEVIEWW;
		if (*tv).action == TVE_EXPAND
		{
			ui_tree_on_item_expanding((*tv).itemNew.hItem);
		}
		return 0;
	}
	else if code == HDN_ENDTRACKW || code == HDN_ENDTRACKA
	{
		if let Some(hlist) = ui_find_listview_by_header((*nmhdr).hwndFrom)
		{
			ui_update_playlist_col_ratios_from_listview(hlist);
		}
		return 0;
	}
	else if code == NM_CUSTOMDRAW
	{
		if let Some(li_id) = ui_find_playlist_id_by_hlist((*nmhdr).hwndFrom)
		{
			let cd = lparam as *mut NMLVCUSTOMDRAW;
			let stage = (*cd).nmcd.dwDrawStage;

			if stage == CDDS_PREPAINT
			{
				return CDRF_NOTIFYITEMDRAW as i64;
			}
			if stage == CDDS_ITEMPREPAINT
			{
				if li_id == UI_CURRENT_PLAYING_LI_ID
					&& UI_CURRENT_PLAYING_IDX >= 0
					&& (*cd).nmcd.dwItemSpec as i32 == UI_CURRENT_PLAYING_IDX
				{
					return CDRF_NOTIFYSUBITEMDRAW as i64;
				}
				return CDRF_DODEFAULT as i64;
			}
			if stage == (CDDS_ITEMPREPAINT | CDDS_SUBITEM)
			{
				if li_id == UI_CURRENT_PLAYING_LI_ID
					&& UI_CURRENT_PLAYING_IDX >= 0
					&& (*cd).nmcd.dwItemSpec as i32 == UI_CURRENT_PLAYING_IDX
				{
					(*cd).clrTextBk = UI_NOW_PLAYING_BK_COLOR;
					(*cd).clrText = UI_NOW_PLAYING_TEXT_COLOR;
					return CDRF_NEWFONT as i64;
				}
			}
			return CDRF_DODEFAULT as i64;
		}
		return CDRF_DODEFAULT as i64;
	}
	else if code == LVN_GETDISPINFOW
	{
		// 虚拟列表数据回调
		if let Some(li_id) = ui_find_playlist_id_by_hlist((*nmhdr).hwndFrom)
		{
			let disp = lparam as *mut NMLVDISPINFOW;
			ui_fill_playlist_dispinfo(li_id, &mut (*disp).item);
		}
		return 0;
	}
	else if code == LVN_COLUMNCLICK
	{
		// ListView header click: sort by column
		let nmlv = lparam as *const NMLISTVIEW;
		if nmlv.is_null()
		{
			return 0;
		}

		let col = (*nmlv).iSubItem;
		if col < 0
		{
			return 0;
		}

		if let Some(li_id) = ui_find_playlist_id_by_hlist((*nmhdr).hwndFrom)
		{
			if let Some(view_idx) = ui_find_playlist_view_index_by_id(li_id)
			{
				if let Some(view) = ui_pl_views_li.get_mut(view_idx)
				{
					// “*” 列：默认不排序；由 ui_pl_sort 设置 mode 后才启用（例如 mode=1 -> file_size）。
					if col == UI_PLAYLIST_INFO_COL_SUBITEM && ui_info_sort_mode() == UI_INFO_SORT_MODE_NONE
					{
						return 0;
					}

					if view.sort_col == col
					{
						view.sort_asc = !view.sort_asc;
					}
					else
					{
						view.sort_col = col;
						view.sort_asc = true;
					}

					if col == UI_PLAYLIST_INFO_COL_SUBITEM
					{
						match ui_info_sort_mode()
						{
							UI_INFO_SORT_MODE_FILE_SIZE => ui_playlist_sort_by_file_size(li_id, view.sort_asc),
							UI_INFO_SORT_MODE_PATH => ui_playlist_sort_by_path(li_id, view.sort_asc),
							_ =>
							{}
						}
					}
					else
					{
						ui_playlist_sort_by_column(li_id, col, view.sort_asc);
					}

					let count = SendMessageW(view.hlist, LVM_GETITEMCOUNT, 0, 0) as i32;
					if count > 0
					{
						SendMessageW(view.hlist, LVM_REDRAWITEMS, 0, (count - 1) as i64);
					}
				}
			}
		}

		return 0;
	}
	else if code == NM_DBLCLK
	{
		// ListView 双击
		if let Some(target_li_id) = ui_find_playlist_id_by_hlist((*nmhdr).hwndFrom)
		{
			let nmia = lparam as *const NMITEMACTIVATE;
			let clicked_idx = (*nmia).iItem;
			if clicked_idx >= 0
			{
				let active_li_id = g_li_id.load(Ordering::SeqCst);
				if target_li_id == active_li_id
				{
					let current_track = g_track.load(Ordering::SeqCst);
					if clicked_idx as usize == current_track
					{
						// 双击当前播放项-> 暂停/恢复
						PostMessageW(G_HWND, WM_RESTART, 0, 0);
						resume_if_paused();
					}
					else
					{
						// 双击其他项-> 切换到该项
						push_pl_cmd(PlayerCommand::SwitchToIndex(clicked_idx as usize));
						resume_if_paused();
					}
				}
				else if set_active_playlist(target_li_id, clicked_idx as usize)
				{
					// 切换播放列表并立即开始播放
					resume_if_paused();
				}
			}
		}
	}

	0
}

// src\ui_pl_sort.rs
// 自定义播放列表排序 + “*” 列（info）运行时模式
//
// sort mode:
// 0 = disabled (default): 点击“*”列不触发排序，显示为空
// 1 = file size: 点击“*”列按文件大小排序，显示文件大小
// 2 = path: 点击“*”列按路径排序，显示路径字符串

const UI_INFO_SORT_MODE_NONE: i32 = 0;
const UI_INFO_SORT_MODE_FILE_SIZE: i32 = 1;
const UI_INFO_SORT_MODE_PATH: i32 = 2;

static UI_INFO_SORT_MODE: AtomicI32 = AtomicI32::new(UI_INFO_SORT_MODE_NONE);

fn ui_info_sort_mode() -> i32 {
	UI_INFO_SORT_MODE.load(Ordering::SeqCst)
}

unsafe fn ui_set_info_sort_mode(mode: i32) {
	let mode = match mode
	{
		UI_INFO_SORT_MODE_FILE_SIZE => UI_INFO_SORT_MODE_FILE_SIZE,
		UI_INFO_SORT_MODE_PATH => UI_INFO_SORT_MODE_PATH,
		_ => UI_INFO_SORT_MODE_NONE,
	};
	UI_INFO_SORT_MODE.store(mode, Ordering::SeqCst);

	// Refresh all list views (ownerdata: text is pulled on redraw).
	for v in ui_pl_views_li.iter()
	{
		if v.hlist != 0
		{
			InvalidateRect(v.hlist, null(), 1);
		}
	}
}

fn ui_format_file_size(bytes: u64) -> String {
	if bytes == 0
	{
		return String::new();
	}

	const KB: f64 = 1024.0;
	const MB: f64 = 1024.0 * 1024.0;
	const GB: f64 = 1024.0 * 1024.0 * 1024.0;

	let b = bytes as f64;
	if b >= GB
	{
		return format!("{:.2} GB", b / GB);
	}
	if b >= MB
	{
		return format!("{:.2} MB", b / MB);
	}
	if b >= KB
	{
		return format!("{:.1} KB", b / KB);
	}
	format!("{} B", bytes)
}

fn ui_info_col_text(song: &SongInfo) -> String {
	match ui_info_sort_mode()
	{
		UI_INFO_SORT_MODE_FILE_SIZE => ui_format_file_size(song.file_size),
		UI_INFO_SORT_MODE_PATH => song.path.clone(),
		_ => String::new(),
	}
}

fn ui_cmp_file_size(a: u64, b: u64) -> std::cmp::Ordering {
	let a = if a == 0 { u64::MAX } else { a };
	let b = if b == 0 { u64::MAX } else { b };
	a.cmp(&b)
}

unsafe fn ui_playlist_sort_by_file_size(li_id: usize, asc: bool) {
	let playing_path_key = NOW_PLAYING
		.read()
		.ok()
		.map(|np| normalize_path_key(&np.path))
		.unwrap_or_default();

	let mut pool = m_pl_pool.write().unwrap();
	let Some(items) = pool.get_mut(&li_id)
	else
	{
		return;
	};

	if items.len() <= 1
	{
		return;
	}

	items.sort_by(|a, b| {
		let mut ord = ui_cmp_file_size(a.file_size, b.file_size);
		if ord == std::cmp::Ordering::Equal
		{
			ord = ui_playlist_cmp_text(&a.path, &b.path);
		}
		if asc { ord } else { ord.reverse() }
	});

	let _ = db_replace_playlist(li_id, None, items);

	// Keep "now playing" marker aligned when sorting the active playlist.
	if li_id == g_li_id.load(Ordering::SeqCst) && !playing_path_key.is_empty()
	{
		if let Some(new_idx) = items
			.iter()
			.position(|s| normalize_path_key(&s.path) == playing_path_key)
		{
			g_track.store(new_idx, Ordering::SeqCst);
			UI_CURRENT_PLAYING_LI_ID = li_id;
			UI_CURRENT_PLAYING_IDX = new_idx as i32;
			ui_playlist_select(li_id, new_idx);
		}
	}
}

unsafe fn ui_playlist_sort_by_path(li_id: usize, asc: bool) {
	let playing_path_key = NOW_PLAYING
		.read()
		.ok()
		.map(|np| normalize_path_key(&np.path))
		.unwrap_or_default();

	let mut pool = m_pl_pool.write().unwrap();
	let Some(items) = pool.get_mut(&li_id)
	else
	{
		return;
	};

	if items.len() <= 1
	{
		return;
	}

	items.sort_by(|a, b| {
		let mut ord = ui_playlist_cmp_text(&a.path, &b.path);
		if ord == std::cmp::Ordering::Equal
		{
			ord = a.path.cmp(&b.path);
		}
		if asc { ord } else { ord.reverse() }
	});

	let _ = db_replace_playlist(li_id, None, items);

	// Keep "now playing" marker aligned when sorting the active playlist.
	if li_id == g_li_id.load(Ordering::SeqCst) && !playing_path_key.is_empty()
	{
		if let Some(new_idx) = items
			.iter()
			.position(|s| normalize_path_key(&s.path) == playing_path_key)
		{
			g_track.store(new_idx, Ordering::SeqCst);
			UI_CURRENT_PLAYING_LI_ID = li_id;
			UI_CURRENT_PLAYING_IDX = new_idx as i32;
			ui_playlist_select(li_id, new_idx);
		}
	}
}

// UI message entry: 40007
// sort_id:
// 0 = disable “*” column sort/display
// 1 = file size sort (immediate) + “*” shows size
// 2 = path sort (immediate) + “*” shows path
unsafe fn ui_pl_sort(pl_id: usize, sort_id: i64) -> i64 {
	let mode = match sort_id
	{
		1 => UI_INFO_SORT_MODE_FILE_SIZE,
		2 => UI_INFO_SORT_MODE_PATH,
		_ => UI_INFO_SORT_MODE_NONE,
	};

	ui_set_info_sort_mode(mode);

	if mode == UI_INFO_SORT_MODE_NONE
	{
		return 0;
	}

	if let Some(view_idx) = ui_find_playlist_view_index_by_id(pl_id)
	{
		if let Some(view) = ui_pl_views_li.get_mut(view_idx)
		{
			// Treat programmatic sort as if the user clicked this column once.
			view.sort_col = UI_PLAYLIST_INFO_COL_SUBITEM;
			view.sort_asc = true;
		}
	}

	match mode
	{
		UI_INFO_SORT_MODE_FILE_SIZE => ui_playlist_sort_by_file_size(pl_id, true),
		UI_INFO_SORT_MODE_PATH => ui_playlist_sort_by_path(pl_id, true),
		_ =>
		{}
	}

	// Redraw the target list view.
	let hlist = ui_listview_for_li_id(pl_id);
	if hlist != 0
	{
		let count = SendMessageW(hlist, LVM_GETITEMCOUNT, 0, 0) as i32;
		if count > 0
		{
			SendMessageW(hlist, LVM_REDRAWITEMS, 0, (count - 1) as i64);
		}
		else
		{
			InvalidateRect(hlist, null(), 1);
		}
	}
	0
}

// src\ui_playlist.rs
unsafe fn ui_init_playlist_listview(hlist: i64) {
	SendMessageW(hlist, WM_SETFONT, UI_HFONT as usize, 1);
	// 设置扩展样式: 整行选中 + 双缓冲
	SendMessageW(hlist, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as i64);

	// 添加列: 编号, 歌曲名, 时长, 作者, 专辑, info(运行时)
	let widths = ui_playlist_col_widths_for_init(hlist);
	let cols: [(&str, i32, i32); UI_PLAYLIST_COL_COUNT_TOTAL] = [
		("#", widths[0], LVCFMT_RIGHT),
		("歌曲名", widths[1], LVCFMT_LEFT),
		("时长", widths[2], LVCFMT_CENTER),
		("作者", widths[3], LVCFMT_LEFT),
		("专辑", widths[4], LVCFMT_LEFT),
		("*", widths[UI_PLAYLIST_INFO_COL_IDX], LVCFMT_LEFT),
	];
	for (i, (name, width, fmt)) in cols.iter().enumerate()
	{
		let text = to_wstring(name);
		let col = LVCOLUMNW {
			mask: LVCF_FMT | LVCF_WIDTH | LVCF_TEXT,
			fmt: *fmt,
			cx: *width,
			pszText: text.as_ptr(),
			cchTextMax: 0,
			iSubItem: i as i32,
			iImage: 0,
			iOrder: 0,
			cxMin: 0,
			cxDefault: 0,
			cxIdeal: 0,
		};
		SendMessageW(hlist, LVM_INSERTCOLUMNW, i, &col as *const _ as i64);
	}
}

const UI_PLAYLIST_COL_INIT_TOTAL_W: i32 = 570;

fn ui_playlist_info_col_width(total_w: i32) -> i32 {
	if total_w <= 0
	{
		return 0;
	}

	(((total_w as i64 * UI_PLAYLIST_INFO_COL_RATIO_DEFAULT as i64) / (UI_PLAYLIST_COL_RATIO_TOTAL as i64)) as i32).max(1)
}

fn ui_normalize_playlist_col_ratios(ratios: &mut [u32; UI_PLAYLIST_COL_COUNT]) {
	let mut sum = 0u32;
	for v in ratios.iter()
	{
		sum = sum.saturating_add(*v);
	}

	if sum == 0
	{
		*ratios = UI_PLAYLIST_COL_RATIO_DEFAULTS;
		return;
	}

	if sum != UI_PLAYLIST_COL_RATIO_TOTAL
	{
		let mut acc = 0u32;
		for i in 0..UI_PLAYLIST_COL_COUNT.saturating_sub(1)
		{
			let v = ((ratios[i] as u64 * UI_PLAYLIST_COL_RATIO_TOTAL as u64) / sum as u64) as u32;
			ratios[i] = v;
			acc = acc.saturating_add(v);
		}
		let last = UI_PLAYLIST_COL_COUNT.saturating_sub(1);
		ratios[last] = UI_PLAYLIST_COL_RATIO_TOTAL.saturating_sub(acc.min(UI_PLAYLIST_COL_RATIO_TOTAL));
	}
}

fn ui_get_playlist_col_ratios() -> [u32; UI_PLAYLIST_COL_COUNT] {
	let mut ratios = [0u32; UI_PLAYLIST_COL_COUNT];
	for i in 0..UI_PLAYLIST_COL_COUNT
	{
		ratios[i] = UI_PLAYLIST_COL_RATIOS[i].load(Ordering::SeqCst);
	}
	ui_normalize_playlist_col_ratios(&mut ratios);
	ratios
}

fn ui_set_playlist_col_ratios(ratios: [u32; UI_PLAYLIST_COL_COUNT]) {
	let mut ratios = ratios;
	ui_normalize_playlist_col_ratios(&mut ratios);
	for i in 0..UI_PLAYLIST_COL_COUNT
	{
		UI_PLAYLIST_COL_RATIOS[i].store(ratios[i], Ordering::SeqCst);
	}
}

fn ui_playlist_col_widths_from_ratios(total_w: i32, ratios: &[u32; UI_PLAYLIST_COL_COUNT]) -> [i32; UI_PLAYLIST_COL_COUNT] {
	let mut widths = [0i32; UI_PLAYLIST_COL_COUNT];
	if total_w <= 0
	{
		return widths;
	}

	let total = UI_PLAYLIST_COL_RATIO_TOTAL as i64;
	let mut used = 0i32;
	for i in 0..UI_PLAYLIST_COL_COUNT.saturating_sub(1)
	{
		let w = ((total_w as i64 * ratios[i] as i64) / total) as i32;
		widths[i] = w.max(1);
		used = used.saturating_add(widths[i]);
	}
	let last = UI_PLAYLIST_COL_COUNT.saturating_sub(1);
	widths[last] = (total_w - used).max(1);
	widths
}

unsafe fn ui_playlist_col_widths_for_init(hlist: i64) -> [i32; UI_PLAYLIST_COL_COUNT_TOTAL] {
	let ratios = ui_get_playlist_col_ratios();
	let mut total_w = UI_PLAYLIST_COL_INIT_TOTAL_W;
	if hlist != 0
	{
		let mut rc: RECT = zeroed();
		if GetClientRect(hlist, &mut rc) != 0
		{
			let w = (rc.right - rc.left).max(0);
			if w > 0
			{
				total_w = w;
			}
		}
	}

	let info_w = ui_playlist_info_col_width(total_w);
	let base_w = (total_w - info_w).max(0);
	let base_widths = ui_playlist_col_widths_from_ratios(base_w, &ratios);

	let mut widths = [0i32; UI_PLAYLIST_COL_COUNT_TOTAL];
	for i in 0..UI_PLAYLIST_COL_COUNT
	{
		widths[i] = base_widths[i];
	}
	widths[UI_PLAYLIST_INFO_COL_IDX] = info_w;
	widths
}

unsafe fn ui_apply_playlist_column_ratios(hlist: i64) -> bool {
	if hlist == 0
	{
		return false;
	}
	let mut rc: RECT = zeroed();
	if GetClientRect(hlist, &mut rc) == 0
	{
		return false;
	}
	let total_w = (rc.right - rc.left).max(0);
	if total_w <= 0
	{
		return false;
	}

	let ratios = ui_get_playlist_col_ratios();
	let info_w = ui_playlist_info_col_width(total_w);
	let base_w = (total_w - info_w).max(0);
	let widths = ui_playlist_col_widths_from_ratios(base_w, &ratios);
	for i in 0..UI_PLAYLIST_COL_COUNT
	{
		SendMessageW(hlist, LVM_SETCOLUMNWIDTH, i, widths[i] as i64);
	}
	SendMessageW(hlist, LVM_SETCOLUMNWIDTH, UI_PLAYLIST_INFO_COL_IDX, info_w as i64);
	true
}

unsafe fn ui_apply_playlist_column_ratios_all() {
	for v in ui_pl_views_li.iter()
	{
		if v.hlist != 0
		{
			let _ = ui_apply_playlist_column_ratios(v.hlist);
		}
	}
}

unsafe fn ui_mark_playlist_columns_pending_apply() {
	for v in ui_pl_views_li.iter_mut()
	{
		v.cols_inited = false;
		v.cols_last_total_w = 0;
	}
}

unsafe fn ui_update_playlist_col_ratios_from_listview(hlist: i64) {
	if hlist == 0
	{
		return;
	}

	let mut widths = [0i32; UI_PLAYLIST_COL_COUNT];
	let mut total = 0i32;
	for i in 0..UI_PLAYLIST_COL_COUNT
	{
		let w = SendMessageW(hlist, LVM_GETCOLUMNWIDTH, i, 0) as i32;
		let w = w.max(0);
		widths[i] = w;
		total = total.saturating_add(w);
	}

	if total <= 0
	{
		return;
	}

	let mut ratios = [0u32; UI_PLAYLIST_COL_COUNT];
	let mut acc = 0u32;
	for i in 0..UI_PLAYLIST_COL_COUNT.saturating_sub(1)
	{
		let v = ((widths[i] as i64 * UI_PLAYLIST_COL_RATIO_TOTAL as i64) / total as i64) as u32;
		ratios[i] = v;
		acc = acc.saturating_add(v);
	}
	let last = UI_PLAYLIST_COL_COUNT.saturating_sub(1);
	ratios[last] = UI_PLAYLIST_COL_RATIO_TOTAL.saturating_sub(acc.min(UI_PLAYLIST_COL_RATIO_TOTAL));

	ui_set_playlist_col_ratios(ratios);
	db_save_playlist_column_ratios(&ratios);
}

unsafe fn ui_find_listview_by_header(header_hwnd: i64) -> Option<i64> {
	if header_hwnd == 0
	{
		return None;
	}
	for v in ui_pl_views_li.iter()
	{
		if v.hlist == 0
		{
			continue;
		}
		let hheader = SendMessageW(v.hlist, LVM_GETHEADER, 0, 0) as i64;
		if hheader == header_hwnd
		{
			return Some(v.hlist);
		}
	}
	None
}

fn play_mode_to_str(play_mode: usize) -> &'static str {
	match play_mode
	{
		0 => "单曲",
		1 => "随机",
		2 => "顺序",
		_ => "顺序",
	}
}

unsafe fn ui_playlist_name_by_id(playlist_id: usize) -> String {
	if playlist_id == 0
	{
		return "(none)".to_string();
	}

	if let Some(idx) = ui_find_playlist_view_index_by_id(playlist_id)
	{
		return ui_pl_views_li[idx].name.clone();
	}

	for (id, name) in db_load_playlists()
	{
		if id == playlist_id
		{
			return name;
		}
	}

	format!("playlist {}", playlist_id)
}

unsafe fn ui_format_playlist_info(playlist_id: usize) -> String {
	if playlist_id == 0
	{
		return "id=0".to_string();
	}

	let name = ui_playlist_name_by_id(playlist_id);
	let play_mode = db_load_playlist_play_mode(playlist_id);
	format!("id={}, name={}, mode={}({})", playlist_id, name, play_mode, play_mode_to_str(play_mode))
}

unsafe fn ui_playlist_len_by_id(playlist_id: usize) -> usize {
	if playlist_id == 0
	{
		return 0;
	}

	if let Ok(pool) = m_pl_pool.read()
		&& let Some(li) = pool.get(&playlist_id)
	{
		return li.len();
	}

	db_load_playlist_items(playlist_id).len()
}

unsafe fn ui_log_playlist_switch(reason: &str, from_playlist_id: usize, to_playlist_id: usize, start_idx: Option<usize>) {
	let _ = reason;
	let _ = from_playlist_id;

	let name = ui_playlist_name_by_id(to_playlist_id);
	let play_mode = db_load_playlist_play_mode(to_playlist_id);
	let total = ui_playlist_len_by_id(to_playlist_id);

	if let Some(idx) = start_idx
	{
		eprintln!(
			"[ui] to-pl: id={}, name={}, mode={}({}), idx={}/{}",
			to_playlist_id,
			name,
			play_mode,
			play_mode_to_str(play_mode),
			idx,
			total
		);
	}
	else
	{
		eprintln!("[ui] to-pl: id={}, name={}, mode={}({})", to_playlist_id, name, play_mode, play_mode_to_str(play_mode),);
	}
}

struct UiPlaylistView {
	playlist_id: usize,
	name: String,
	hlist: i64,
	hheader: i64,
	cols_inited: bool,
	cols_last_total_w: i32,
	sort_col: i32,
	sort_asc: bool,
}

static mut ui_pl_views_li: Vec<UiPlaylistView> = Vec::new();

unsafe fn ui_find_playlist_view_index_by_id(playlist_id: usize) -> Option<usize> {
	ui_pl_views_li
		.iter()
		.position(|v| v.playlist_id == playlist_id)
}

unsafe fn ui_find_playlist_id_by_hlist(hwnd_list: i64) -> Option<usize> {
	ui_pl_views_li
		.iter()
		.find(|v| v.hlist == hwnd_list)
		.map(|v| v.playlist_id)
}

unsafe fn ui_append_playlist_view(playlist_id: usize, name: &str) -> Option<usize> {
	let display_name = if name.trim().is_empty() { format!("playlist {}", playlist_id) } else { name.to_string() };
	let idx = ui_pl_views_li.len();

	let text = to_wstring(&display_name);
	let mut item: TCITEMW = zeroed();
	item.mask = TCIF_TEXT;
	item.pszText = text.as_ptr();
	SendMessageW(UI_HTAB, TCM_INSERTITEMW, idx, &item as *const _ as i64);

	let listview_class = to_wstring("SysListView32");
	let hlist = CreateWindowExW(
		0,
		listview_class.as_ptr(),
		null_mut(),
		WS_CHILD | WS_VSCROLL | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_OWNERDATA,
		0,
		PROGRESS_TOTAL_HEIGHT + TAB_HEIGHT,
		0,
		0,
		UI_HWND,
		ID_LISTVIEW_BASE + idx as i64,
		0,
		0,
	);
	if hlist != 0
	{
		ui_init_playlist_listview(hlist);
	}
	ui_pl_views_li.push(UiPlaylistView {
		playlist_id,
		name: display_name,
		hlist,
		hheader: SendMessageW(hlist, LVM_GETHEADER, 0, 0),
		cols_inited: false,
		cols_last_total_w: 0,
		sort_col: -1,
		sort_asc: true,
	});
	Some(idx)
}

unsafe fn ui_ensure_playlist_view(playlist_id: usize) -> Option<usize> {
	if let Some(idx) = ui_find_playlist_view_index_by_id(playlist_id)
	{
		return Some(idx);
	}

	let mut name = None;
	for (id, pl_name) in db_load_playlists()
	{
		if id == playlist_id
		{
			name = Some(pl_name);
			break;
		}
	}
	let name = name.unwrap_or_else(|| format!("playlist {}", playlist_id));
	let idx = ui_append_playlist_view(playlist_id, &name)?;
	ui_layout(UI_HWND);
	Some(idx)
}

unsafe fn ui_apply_tab_selection(sel: i32) {
	if ui_pl_views_li.is_empty()
	{
		return;
	}
	let mut sel = sel.max(0) as usize;
	if sel >= ui_pl_views_li.len()
	{
		sel = 0;
	}

	for (i, v) in ui_pl_views_li.iter().enumerate()
	{
		if i == sel
		{
			UI_HLIST = v.hlist;
			ShowWindow(v.hlist, SW_SHOW);
		}
		else
		{
			ShowWindow(v.hlist, 0);
		}
	}

	// tab 切换后按当前 ListView 宽度重算列宽（按比例），保证窗口尺寸变化后仍能铺满。
	if let Some(v) = ui_pl_views_li.get_mut(sel)
	{
		if v.hlist != 0
		{
			let mut rc: RECT = zeroed();
			if GetClientRect(v.hlist, &mut rc) != 0
			{
				let total_w = (rc.right - rc.left).max(0);
				if total_w > 0 && (!v.cols_inited || v.cols_last_total_w != total_w)
				{
					if ui_apply_playlist_column_ratios(v.hlist)
					{
						v.cols_inited = true;
						v.cols_last_total_w = total_w;
					}
				}
			}
		}
	}
	if UI_HLIST != 0
	{
		SetFocus(UI_HLIST);
	}
}

unsafe fn ui_listview_for_li_id(li_id: usize) -> i64 {
	if let Some(i) = ui_find_playlist_view_index_by_id(li_id) { ui_pl_views_li[i].hlist } else { 0 }
}

unsafe fn ui_pl_id_to_play(target_li_id: usize, clicked_idx: usize) {
	let active_li_id = g_li_id.load(Ordering::SeqCst);
	let ui_li_id = {
		let sel = SendMessageW(UI_HTAB, TCM_GETCURSEL, 0, 0) as i32;
		if sel >= 0 && (sel as usize) < ui_pl_views_li.len() { ui_pl_views_li[sel as usize].playlist_id } else { 0 }
	};

	// “当前列表”有两个语义：
	// 1) UI 正在显示的列表（当前 tab）
	// 2) 正在播放的列表（active_li_id）
	// 只有当 1) 与 2) 相同，并且目标也是该列表时，才跳过“切换播放列表”的操作。
	if target_li_id == active_li_id && target_li_id == ui_li_id
	{
		let current_track = g_track.load(Ordering::SeqCst);
		if clicked_idx == current_track
		{
			// 双击当前播放项-> 重新播放
			PostMessageW(G_HWND, WM_RESTART, 0, 0);
			resume_if_paused();
		}
		else
		{
			// 双击其他项-> 切换到该项
			push_pl_cmd(PlayerCommand::SwitchToIndex(clicked_idx));
			resume_if_paused();
		}
	}
	else if target_li_id == active_li_id
	{
		// 目标是“正在播放的列表”，但 UI 当前显示的是另一个列表：只切换 UI，不重置播放列表状态。
		ui_log_playlist_switch("dblclick", ui_li_id, target_li_id, Some(clicked_idx));
		ui_sync_playlist_tabs(target_li_id);

		let current_track = g_track.load(Ordering::SeqCst);
		if clicked_idx == current_track
		{
			PostMessageW(G_HWND, WM_RESTART, 0, 0);
			resume_if_paused();
		}
		else
		{
			push_pl_cmd(PlayerCommand::SwitchToIndex(clicked_idx));
			resume_if_paused();
		}
	}
	else
	{
		ui_log_playlist_switch("dblclick", active_li_id, target_li_id, Some(clicked_idx));
		if set_active_playlist(target_li_id, clicked_idx)
		{
			// 切换播放列表并立即开始播放
			resume_if_paused();
		}
		else
		{
			eprintln!("[ui] 切换播放列表失败: target_id={}, start_idx={}", target_li_id, clicked_idx);
		}
	}
}

unsafe fn ui_pl_reset_from_id(pl_id: usize) {
	let songs = db_load_playlist_items(pl_id);

	let songs_for_ui = songs.clone();
	{
		let mut pool = m_pl_pool.write().unwrap();
		pool.insert(pl_id, songs);
	}

	let ui_refresh_now_playing = if UI_CURRENT_PLAYING_LI_ID == pl_id && UI_CURRENT_PLAYING_IDX >= 0
	{
		if songs_for_ui.is_empty()
		{
			None
		}
		else
		{
			let idx = (UI_CURRENT_PLAYING_IDX as usize).min(songs_for_ui.len().saturating_sub(1));
			let song = &songs_for_ui[idx];
			Some((idx, song.clone()))
		}
	}
	else
	{
		None
	};

	ui_playlist_update(pl_id, songs_for_ui);

	if let Some((idx, song)) = ui_refresh_now_playing
	{
		ui_set_now_playing2(pl_id, idx, &song);
	}

	eprintln!("[ui] 40009 reload playlist: id={}", pl_id);
}

// src\ui_progress.rs
/// 进度条子类过程 - 处理点击事件
unsafe extern "system" fn progress_subclass_proc(hwnd: i64, msg: u32, wparam: usize, lparam: i64) -> i64 {
	if msg == WM_LBUTTONDOWN
	{
		// 获取点击位置 (lparam 低 16 位是 x)
		let x = (lparam & 0xFFFF) as i32;

		// 获取进度条宽度
		let mut rc: RECT = zeroed();
		GetClientRect(hwnd, &mut rc);
		let width = rc.right - rc.left;

		if width > 0
		{
			let ratio = (x as f64) / (width as f64);
			let ratio = ratio.clamp(0.0, 1.0);

			let total_ms = TRACK_DURATION_MS.load(Ordering::SeqCst);
			if total_ms > 0
			{
				let seek_ms = (ratio * total_ms as f64) as u64;

				// 播放中：交给 playback 线程做淡出/静音/淡入，避免 seek 瞬态咔嚓
				// 暂停中：playback 线程不会处理 seek，因此直接唤醒解码线程执行 seek
				let is_paused = WaitForSingleObject(g_ev_resume, 0) == 258;
				if is_paused
				{
					g_seek_to_ms.store(seek_ms as i64, Ordering::SeqCst);
					SetEvent(g_ev_dec_wakeup);
				}
				else
				{
					g_seek_req_ms.store(seek_ms as i64, Ordering::SeqCst);
				}

				// 立即更新进度条 UI 位置（千分比）
				let pos = ((seek_ms * 1000) / total_ms).min(1000) as i32;
				SendMessageW(hwnd, PBM_SETPOS, pos as usize, 0);
				InvalidateRect(hwnd, null(), 1);

				// 立即更新播放位置计数器，避免播放线程发送旧的进度更新
				let output_sample_rate = OUTPUT_SAMPLE_RATE.load(Ordering::SeqCst) as u64;
				let channels = OUTPUT_CHANNELS.load(Ordering::SeqCst) as u64;
				if output_sample_rate > 0 && channels > 0
				{
					let samples = (seek_ms * output_sample_rate * channels) / 1000;
					SAMPLES_PLAYED.store(samples, Ordering::SeqCst);
				}
			}
		}
		return 0;
	}
	if msg == WM_ERASEBKGND
	{
		return 1;
	}
	if msg == WM_PAINT
	{
		let (hbr_bg, hbr_bar) = if UI_TOOLBAR_PLAYING
		{
			(UI_HBRUSH_PROGRESS_PLAY_BG, UI_HBRUSH_PROGRESS_PLAY_BAR)
		}
		else
		{
			(UI_HBRUSH_PROGRESS_PAUSE_BG, UI_HBRUSH_PROGRESS_PAUSE_BAR)
		};
		if hbr_bg == 0 || hbr_bar == 0
		{
			return CallWindowProcW(UI_PROGRESS_OLDPROC, hwnd, msg, wparam, lparam);
		}
		let mut ps: PAINTSTRUCT = zeroed();
		let hdc = BeginPaint(hwnd, &mut ps);
		if hdc != 0
		{
			let mut rc_all: RECT = zeroed();
			GetClientRect(hwnd, &mut rc_all);

			// Fill full hit area with window background, then draw a slimmer bar centered vertically.
			FillRect(hdc, &rc_all as *const RECT, COLOR_WINDOW + 1);

			let width = (rc_all.right - rc_all.left).max(0);
			let height = (rc_all.bottom - rc_all.top).max(0);
			let bar_h = PROGRESS_HEIGHT.min(height);
			let bar_top = rc_all.top + ((height - bar_h) / 2).max(0);
			let rc = RECT { left: rc_all.left, top: bar_top, right: rc_all.right, bottom: bar_top + bar_h };

			FillRect(hdc, &rc as *const RECT, hbr_bg);
			if width > 0
			{
				let pos = SendMessageW(hwnd, PBM_GETPOS, 0, 0) as i32;
				let pos = pos.clamp(0, 1000);
				let fill_w = ((width as i64 * pos as i64) / 1000) as i32;
				if fill_w > 0
				{
					let fill = RECT { left: rc.left, top: rc.top, right: rc.left + fill_w, bottom: rc.bottom };
					FillRect(hdc, &fill as *const RECT, hbr_bar);
				}
			}
		}
		EndPaint(hwnd, &ps as *const PAINTSTRUCT);
		return 0;
	}
	CallWindowProcW(UI_PROGRESS_OLDPROC, hwnd, msg, wparam, lparam)
}

// src\ui_tab.rs
/// 保存当前播放列表的播放进度到数据库
/// 在切换播放列表前调用，确保当前进度不丢失
unsafe fn save_current_progress() {
	let playlist_id = NOW_PLAYING_LI_ID.load(Ordering::SeqCst);
	if playlist_id == 0
	{
		return;
	}

	let track_path = NOW_PLAYING
		.read()
		.ok()
		.map(|np| np.path.clone())
		.unwrap_or_default();

	if track_path.is_empty()
	{
		return;
	}

	let samples = SAMPLES_PLAYED.load(Ordering::Relaxed);
	let sample_rate = OUTPUT_SAMPLE_RATE.load(Ordering::Relaxed);
	let channels = OUTPUT_CHANNELS.load(Ordering::Relaxed);

	if sample_rate == 0 || channels == 0
	{
		return;
	}

	let progress_ms = (samples * 1000) / (sample_rate * channels) as u64;

	let track_idx = {
		let key = normalize_path_key(&track_path);
		if let Ok(pool) = m_pl_pool.read()
			&& let Some(li) = pool.get(&playlist_id)
		{
			li.iter()
				.position(|s| normalize_path_key(&s.path) == key)
				.unwrap_or(0)
		}
		else
		{
			0
		}
	};

	db_update_progress(playlist_id, track_idx, Some(track_path.as_str()), progress_ms);
}

unsafe fn ui_tab_xy_to_id(x: i32, y: i32) -> Option<usize> {
	let mut ht: TCHITTESTINFO = zeroed();
	ht.pt.x = x;
	ht.pt.y = y;
	let idx = SendMessageW(UI_HTAB, TCM_HITTEST, 0, &mut ht as *mut _ as i64) as i32;
	if idx < 0 { None } else { Some(idx as usize) }
}

unsafe fn ui_tab_id_to_pl(tab_idx: usize) -> bool {
	if tab_idx >= ui_pl_views_li.len()
	{
		return false;
	}

	let li_id = ui_pl_views_li[tab_idx].playlist_id;
	let active_li_id = g_li_id.load(Ordering::SeqCst);
	let is_switch_playlist = li_id != active_li_id;

	// 如果目标选项卡是当前活动播放列表，根据播放状态决定行为
	if li_id == active_li_id
	{
		let state = get_player_state();
		match state
		{
			PlayerState::Playing =>
			{
				// 正在播放，不作为
				return true;
			}
			PlayerState::Paused =>
			{
				// 暂停中，恢复播放
				resume_if_paused();
				return true;
			}
			_ =>
			{
				// 中止/空闲/错误状态，播放最后记录或首项
				// 继续执行下面的恢复逻辑
			}
		}
	}
	else
	{
		// 切换到不同的播放列表前，先保存当前播放列表的播放进度
		save_current_progress();
	}

	// Ensure playlist exists in memory for index resolution / empty checks.
	let li_len = if let Ok(pool) = m_pl_pool.read()
		&& let Some(li) = pool.get(&li_id)
	{
		li.len()
	}
	else
	{
		let songs = db_load_playlist_items(li_id);
		let len = songs.len();
		let mut pool = m_pl_pool.write().unwrap();
		pool.insert(li_id, songs);
		len
	};

	if li_len == 0
	{
		return false;
	}

	let resume = db_load_playlist_resume(li_id);

	let mut start_idx = 0usize;
	let mut start_ms = 0u64;
	let mut resume_path: Option<String> = None;

	if let Some((path, progress_ms)) = resume
	{
		let path = path.trim().to_string();
		if !path.is_empty()
		{
			let key = normalize_path_key(&path);
			let found = if let Ok(pool) = m_pl_pool.read()
				&& let Some(li) = pool.get(&li_id)
			{
				li.iter()
					.position(|s| normalize_path_key(&s.path) == key)
			}
			else
			{
				None
			};

			if let Some(i) = found
			{
				start_idx = i;
				start_ms = progress_ms;
				resume_path = Some(path);
			}
		}
	}

	if let Some(path) = resume_path.as_deref()
	{
		set_pending_track_by_path(li_id, path);
	}

	if is_switch_playlist
	{
		ui_log_playlist_switch("tab", active_li_id, li_id, Some(start_idx));
	}
	if !set_active_playlist_with_resume(li_id, start_idx, start_ms)
	{
		return false;
	}

	resume_if_paused();
	true
}

unsafe fn ui_tab_delete_playlist_at(tab_idx: usize) -> bool {
	if tab_idx >= ui_pl_views_li.len()
	{
		return false;
	}

	let li_id = ui_pl_views_li[tab_idx].playlist_id;
	if li_id == PLAYLIST_ID_USER || li_id == PLAYLIST_ID_DEFAULT
	{
		return false;
	}

	let cur_sel = SendMessageW(UI_HTAB, TCM_GETCURSEL, 0, 0) as i32;
	let name = ui_pl_views_li[tab_idx].name.clone();
	let hlist = ui_pl_views_li[tab_idx].hlist;

	if !db_delete_playlist(li_id)
	{
		eprintln!("[ui] delete playlist failed: id={}, name={}", li_id, name);
		return false;
	}

	{
		let mut pool = m_pl_pool.write().unwrap();
		pool.remove(&li_id);
	}

	if hlist != 0
	{
		DestroyWindow(hlist);
	}

	SendMessageW(UI_HTAB, TCM_DELETEITEM, tab_idx, 0);
	ui_pl_views_li.remove(tab_idx);

	let mut new_sel = cur_sel;
	if cur_sel >= 0
	{
		let cur = cur_sel as usize;
		if tab_idx < cur
		{
			new_sel = cur_sel - 1;
		}
	}

	if !ui_pl_views_li.is_empty()
	{
		let max = (ui_pl_views_li.len() - 1) as i32;
		if new_sel < 0
		{
			new_sel = 0;
		}
		if new_sel > max
		{
			new_sel = max;
		}
		SendMessageW(UI_HTAB, TCM_SETCURSEL, new_sel as usize, 0);
		ui_apply_tab_selection(new_sel);
	}
	else
	{
		UI_HLIST = 0;
	}

	ui_layout(UI_HWND);

	if g_li_id.load(Ordering::SeqCst) == li_id
	{
		let fallback = PLAYLIST_ID_USER;
		let mode = db_load_playlist_play_mode(fallback);
		g_pl_mode.store(mode, Ordering::SeqCst);
		g_li_id.store(fallback, Ordering::SeqCst);
		g_track.store(0, Ordering::SeqCst);
		g_pl_is_changed.store(true, Ordering::SeqCst);
		g_to_next.store(false, Ordering::SeqCst);
		g_to_prev.store(false, Ordering::SeqCst);
		if g_ev_li_chang != 0
		{
			SetEvent(g_ev_li_chang);
		}

		UI_CURRENT_PLAYING_LI_ID = fallback;
		UI_CURRENT_PLAYING_IDX = -1;

		let volume = g_to_volume.load(Ordering::SeqCst);
		db_save_state(fallback as i64, 0, None, 0, mode, volume);

		ui_sync_playlist_tabs(fallback);
	}
	else if UI_CURRENT_PLAYING_LI_ID == li_id
	{
		UI_CURRENT_PLAYING_LI_ID = PLAYLIST_ID_USER;
		UI_CURRENT_PLAYING_IDX = -1;
	}

	eprintln!("[ui] deleted playlist: id={}, name={}", li_id, name);
	true
}

unsafe fn ui_tab_clear_user_playlist_at(tab_idx: usize) -> bool {
	if tab_idx >= ui_pl_views_li.len()
	{
		return false;
	}

	let li_id = ui_pl_views_li[tab_idx].playlist_id;
	if li_id != PLAYLIST_ID_USER
	{
		return false;
	}

	if !db_replace_playlist(li_id, None, &[])
	{
		eprintln!("[ui] clear playlist failed: id={}", li_id);
		return false;
	}

	{
		let mut pool = m_pl_pool.write().unwrap();
		pool.insert(li_id, Vec::new());
	}

	ui_playlist_update(li_id, Vec::new());

	// If we're clearing the active playlist, stop playback and reset state.
	if g_li_id.load(Ordering::SeqCst) == li_id
	{
		g_track.store(0, Ordering::SeqCst);
		g_to_pos_ms.store(0, Ordering::SeqCst);
		g_pl_is_changed.store(true, Ordering::SeqCst);
		g_to_next.store(false, Ordering::SeqCst);
		g_to_prev.store(false, Ordering::SeqCst);

		if g_ev_pl_quit != 0
		{
			SetEvent(g_ev_pl_quit);
		}
		if g_ev_li_chang != 0
		{
			SetEvent(g_ev_li_chang);
		}

		UI_CURRENT_PLAYING_LI_ID = li_id;
		UI_CURRENT_PLAYING_IDX = -1;
		set_player_state(PlayerState::Idle);
		db_save_playing_status(false);

		let mode = g_pl_mode.load(Ordering::SeqCst);
		let volume = g_to_volume.load(Ordering::SeqCst);
		db_save_state(li_id as i64, 0, None, 0, mode, volume);
	}
	else if UI_CURRENT_PLAYING_LI_ID == li_id
	{
		UI_CURRENT_PLAYING_IDX = -1;
	}

	eprintln!("[ui] cleared playlist: id={}", li_id);
	true
}

unsafe fn ui_tab_id_to_del(tab_idx: usize) -> bool {
	if tab_idx < ui_pl_views_li.len() && ui_pl_views_li[tab_idx].playlist_id == PLAYLIST_ID_USER
	{
		return ui_tab_clear_user_playlist_at(tab_idx);
	}
	ui_tab_delete_playlist_at(tab_idx)
}

unsafe extern "system" fn tab_subclass_proc(hwnd: i64, msg: u32, wparam: usize, lparam: i64) -> i64 {
	CallWindowProcW(UI_TAB_OLDPROC, hwnd, msg, wparam, lparam)
}

// src\ui_tool.rs
const UI_MIN_LEFT_W: i32 = 240;
const UI_MIN_RIGHT_W: i32 = 220;
const UI_MIN_LIST_H: i32 = 120;
const UI_MIN_LOG_H: i32 = 80;
const UI_MIN_COVER_H: i32 = 60;
const UI_MIN_TREE_H: i32 = 80;

static mut UI_DRAG_MODE: u8 = 0; // 0=none, 1=LR, 2=LIST_LOG, 3=COVER_TREE
static mut UI_SPLIT_LR_OLDPROC: i64 = 0;
static mut UI_SPLIT_LIST_LOG_OLDPROC: i64 = 0;
static mut UI_SPLIT_COVER_TREE_OLDPROC: i64 = 0;

unsafe extern "system" fn splitter_lr_subclass_proc(hwnd: i64, msg: u32, wparam: usize, lparam: i64) -> i64 {
	if msg == WM_LBUTTONDOWN
	{
		UI_DRAG_MODE = 1;
		SetCapture(UI_HWND);
		SetCursor(LoadCursorW(0, IDC_SIZEWE));
		return 0;
	}
	if msg == WM_SETCURSOR
	{
		SetCursor(LoadCursorW(0, IDC_SIZEWE));
		return 1;
	}
	CallWindowProcW(UI_SPLIT_LR_OLDPROC, hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn splitter_list_log_subclass_proc(hwnd: i64, msg: u32, wparam: usize, lparam: i64) -> i64 {
	if msg == WM_LBUTTONDOWN
	{
		UI_DRAG_MODE = 2;
		SetCapture(UI_HWND);
		SetCursor(LoadCursorW(0, IDC_SIZENS));
		return 0;
	}
	if msg == WM_SETCURSOR
	{
		SetCursor(LoadCursorW(0, IDC_SIZENS));
		return 1;
	}
	CallWindowProcW(UI_SPLIT_LIST_LOG_OLDPROC, hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn splitter_cover_tree_subclass_proc(hwnd: i64, msg: u32, wparam: usize, lparam: i64) -> i64 {
	if msg == WM_LBUTTONDOWN
	{
		UI_DRAG_MODE = 3;
		SetCapture(UI_HWND);
		SetCursor(LoadCursorW(0, IDC_SIZENS));
		return 0;
	}
	if msg == WM_SETCURSOR
	{
		SetCursor(LoadCursorW(0, IDC_SIZENS));
		return 1;
	}
	CallWindowProcW(UI_SPLIT_COVER_TREE_OLDPROC, hwnd, msg, wparam, lparam)
}

unsafe fn ui_destroy() {
	ui_cover_shutdown();
	if UI_HFONT != 0
	{
		if UI_HFONT_ICON != 0 && UI_HFONT_ICON != UI_HFONT
		{
			DeleteObject(UI_HFONT_ICON);
		}
		UI_HFONT_ICON = 0;
		DeleteObject(UI_HFONT);
		UI_HFONT = 0;
	}
	if UI_HSPLIT_BRUSH != 0
	{
		DeleteObject(UI_HSPLIT_BRUSH);
		UI_HSPLIT_BRUSH = 0;
	}
	if UI_HBRUSH_PROGRESS_PLAY_BG != 0
	{
		DeleteObject(UI_HBRUSH_PROGRESS_PLAY_BG);
		UI_HBRUSH_PROGRESS_PLAY_BG = 0;
	}
	if UI_HBRUSH_PROGRESS_PAUSE_BG != 0
	{
		DeleteObject(UI_HBRUSH_PROGRESS_PAUSE_BG);
		UI_HBRUSH_PROGRESS_PAUSE_BG = 0;
	}
	if UI_HBRUSH_PROGRESS_PLAY_BAR != 0
	{
		DeleteObject(UI_HBRUSH_PROGRESS_PLAY_BAR);
		UI_HBRUSH_PROGRESS_PLAY_BAR = 0;
	}
	if UI_HBRUSH_PROGRESS_PAUSE_BAR != 0
	{
		DeleteObject(UI_HBRUSH_PROGRESS_PAUSE_BAR);
		UI_HBRUSH_PROGRESS_PAUSE_BAR = 0;
	}
	UI_TOOLBAR_PLAYING = false;
	g_ui_is_visible = false;
	UI_HWND = 0;
	UI_HPROGRESS = 0;
	UI_HTAB = 0;
	UI_HLIST = 0;
	ui_pl_views_li.clear();
	UI_HCOVER = 0;
	UI_HTREE = 0;
	UI_HLOG = 0;
	UI_HSPLIT_LR = 0;
	UI_HSPLIT_LIST_LOG = 0;
	UI_HSPLIT_COVER_TREE = 0;
	UI_DRAG_MODE = 0;
	UI_CURRENT_PLAYING_LI_ID = 0;
	UI_CURRENT_PLAYING_IDX = -1;
	UI_COVER_OLDPROC = 0;
	UI_SPLIT_COVER_TREE_OLDPROC = 0;
}

// src\ui_tree.rs
unsafe extern "system" fn tree_subclass_proc(hwnd: i64, msg: u32, wparam: usize, lparam: i64) -> i64 {
	if msg == WM_MBUTTONDOWN
	{
		let x = (lparam & 0xFFFF) as i16 as i32;
		let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
		if ui_tree_handle_mbutton_click(x, y)
		{
			return 0;
		}
	}
	else if msg == WM_LBUTTONDBLCLK
	{
		let x = (lparam & 0xFFFF) as i16 as i32;
		let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
		if ui_tree_handle_lbutton_dblclk(x, y)
		{
			return 0;
		}
	}
	CallWindowProcW(UI_TREE_OLDPROC, hwnd, msg, wparam, lparam)
}

unsafe fn ui_tree_id_to_pl(hitem: i64) {
	let parts = ui_tree_item_path_parts(hitem);
	if !parts.is_empty()
	{
		let name = parts
			.last()
			.cloned()
			.unwrap_or_default();
		let path = parts.join("\\");
		if !name.is_empty() && !path.is_empty()
		{
			// 判断项类型：有子项为父项，否则为普通项
			let is_folder = ui_tree_item_child(hitem) != 0;
			if is_folder
			{
				// 父项：创建新播放列表 Tab
				ui_tree_midclick_folder_action(name, path);
			}
			else
			{
				// 普通项：添加到"用户"列表并播放
				ui_tree_midclick_file_action(path);
			}
		}
	}
}

unsafe fn ui_tree_item_text(hitem: i64) -> String {
	if hitem == 0 || UI_HTREE == 0
	{
		return String::new();
	}

	let mut buf: Vec<u16> = vec![0; 512];
	let mut item: TVITEMW = zeroed();
	item.mask = TVIF_TEXT;
	item.hItem = hitem;
	item.pszText = buf.as_mut_ptr() as *const u16;
	item.cchTextMax = buf.len() as i32;
	SendMessageW(UI_HTREE, TVM_GETITEMW, 0, &mut item as *mut _ as i64);

	let len = buf
		.iter()
		.position(|&c| c == 0)
		.unwrap_or(buf.len());
	String::from_utf16_lossy(&buf[..len])
		.trim()
		.to_string()
}

unsafe fn ui_tree_item_parent(hitem: i64) -> i64 {
	SendMessageW(UI_HTREE, TVM_GETNEXTITEM, TVGN_PARENT as usize, hitem)
}

unsafe fn ui_tree_item_child(hitem: i64) -> i64 {
	SendMessageW(UI_HTREE, TVM_GETNEXTITEM, TVGN_CHILD as usize, hitem)
}

unsafe fn ui_tree_item_path_parts(mut hitem: i64) -> Vec<String> {
	let mut parts: Vec<String> = Vec::new();
	while hitem != 0
	{
		let t = ui_tree_item_text(hitem);
		if !t.is_empty()
		{
			parts.push(t);
		}
		hitem = ui_tree_item_parent(hitem);
	}
	parts.reverse();
	parts
}
unsafe fn ui_tree_midclick_folder_action(pl_name: String, rel_folder: String) {
	let Some(hdb) = music_db_open_for_query()
	else
	{
		eprintln!("[tree] 无法打开 music.db: {}", MUSIC_DB_PATH);
		return;
	};

	let roots = music_db_collect_scan_roots();
	let mut seen: HashMap<String, ()> = HashMap::default();
	let mut songs: Vec<SongInfo> = Vec::new();

	for (root, _) in roots.iter()
	{
		let prefix = ui_join_root_rel(root, &rel_folder);
		for s in music_db_query_songs_by_dir_prefix(hdb, &prefix)
		{
			let key = normalize_path_key(&s.path);
			if seen.contains_key(&key)
			{
				continue;
			}
			seen.insert(key, ());
			songs.push(s);
		}
	}

	sqlite3_close(hdb);

	if songs.is_empty()
	{
		eprintln!("[tree] 未找到目录歌曲: {}", rel_folder);
		return;
	}

	let pl_name = if pl_name.len() > 30 && songs[0].album.len() < 30 { songs[0].album.to_string() } else { pl_name.trim().to_string() };

	if pl_name.is_empty()
	{
		return;
	}

	let Some(pl_id) = db_get_or_create_playlist_id(Some(pl_name.as_str()))
	else
	{
		eprintln!("[tree] 无法创建/获取播放列表: {}", pl_name);
		return;
	};

	if !db_replace_playlist(pl_id, Some(pl_name.as_str()), &songs)
	{
		eprintln!("[tree] 保存播放列表失败: {}", pl_name);
		return;
	}

	let songs_for_ui = songs.clone();
	{
		let mut pool = m_pl_pool.write().unwrap();
		pool.insert(pl_id, songs);
	}
	ui_playlist_update(pl_id, songs_for_ui);
	ui_sync_playlist_tabs(pl_id);
}

unsafe fn ui_tree_midclick_file_action(rel_file: String) {
	let (rel_dir, name) = split_path(&rel_file);

	let roots = music_db_collect_scan_roots();
	let Some(hdb) = music_db_open_for_query()
	else
	{
		eprintln!("[tree] 无法打开 music.db: {}", MUSIC_DB_PATH);
		return;
	};

	let mut song: Option<SongInfo> = None;
	for (root, _) in roots.iter()
	{
		let abs_dir = ui_join_root_rel(root, rel_dir);
		if let Some(s) = music_db_query_song_by_dir_name(hdb, &abs_dir, name)
		{
			song = Some(s);
			break;
		}
	}
	sqlite3_close(hdb);

	let song = match song
	{
		Some(s) => s,
		None =>
		{
			let fallback_root = roots
				.first()
				.map(|v| v.0.as_str())
				.unwrap_or("");
			let path = ui_join_root_rel(fallback_root, &rel_file);
			collect_song_info(&path)
		}
	};

	let Some(start_idx) = db_append_playlist_items(PLAYLIST_ID_USER, &[song.clone()])
	else
	{
		eprintln!("[tree] 追加到“{}”列表失败", PLAYLIST_NAME_USER);
		return;
	};

	let songs = db_load_playlist_items(PLAYLIST_ID_USER);
	let songs_for_ui = songs.clone();
	{
		let mut pool = m_pl_pool.write().unwrap();
		pool.insert(PLAYLIST_ID_USER, songs);
	}
	ui_playlist_update(PLAYLIST_ID_USER, songs_for_ui);

	if set_active_playlist(PLAYLIST_ID_USER, start_idx)
	{
		resume_if_paused();
	}
}

unsafe fn ui_tree_handle_mbutton_click(x: i32, y: i32) -> bool {
	if UI_HTREE == 0
	{
		return false;
	}

	let mut ht: TVHITTESTINFO = zeroed();
	ht.pt.x = x;
	ht.pt.y = y;

	SendMessageW(UI_HTREE, TVM_HITTEST, 0, &mut ht as *mut _ as i64);
	let hitem = ht.hItem;
	if hitem == 0 || (ht.flags & TVHT_ONITEM) == 0
	{
		return false;
	}

	// 同步选中
	SendMessageW(UI_HTREE, TVM_SELECTITEM, TVGN_CARET as usize, hitem);

	let parts = ui_tree_item_path_parts(hitem);
	if parts.is_empty()
	{
		return false;
	}
	let rel = parts.join("\\");
	let name = parts
		.last()
		.cloned()
		.unwrap_or_default();
	let is_folder = ui_tree_item_child(hitem) != 0;

	thread::spawn(move || unsafe {
		if is_folder
		{
			ui_tree_midclick_folder_action(name, rel);
		}
		else
		{
			ui_tree_midclick_file_action(rel);
		}
	});

	true
}

unsafe fn ui_tree_handle_lbutton_dblclk(x: i32, y: i32) -> bool {
	if UI_HTREE == 0
	{
		return false;
	}

	let mut ht: TVHITTESTINFO = zeroed();
	ht.pt.x = x;
	ht.pt.y = y;
	SendMessageW(UI_HTREE, TVM_HITTEST, 0, &mut ht as *mut _ as i64);
	let hitem = ht.hItem;
	if hitem == 0 || (ht.flags & TVHT_ONITEM) == 0
	{
		return false;
	}

	// 同步选中
	SendMessageW(UI_HTREE, TVM_SELECTITEM, TVGN_CARET as usize, hitem);

	// Only for file nodes (leaf). Folders keep native expand/collapse behavior.
	let is_folder = ui_tree_item_child(hitem) != 0;
	if is_folder
	{
		return false;
	}

	let parts = ui_tree_item_path_parts(hitem);
	if parts.is_empty()
	{
		return false;
	}
	let rel = parts.join("\\");

	thread::spawn(move || unsafe {
		ui_tree_midclick_file_action(rel);
	});

	true
}

fn music_tree_relative_path(roots: &[(String, String)], dir: &str, name: &str) -> String {
	let mut full = String::with_capacity(dir.len() + name.len() + 1);
	full.push_str(dir);
	full.push('\\');
	full.push_str(name);

	let full_fixed = full.replace('/', "\\");

	let mut best_root: Option<&str> = None;
	let mut best_len = 0usize;

	for (root_fixed, root_norm) in roots
	{
		let root_len = root_norm.len();
		if root_len == 0 || full_fixed.len() < root_len
		{
			continue;
		}

		if starts_with_lowercase_prefix(&full_fixed, root_norm)
		{
			if full_fixed.len() == root_len || full_fixed.as_bytes().get(root_len) == Some(&b'\\')
			{
				if root_len > best_len
				{
					best_len = root_len;
					best_root = Some(root_fixed.as_str());
				}
			}
		}
	}

	if let Some(best_root) = best_root
	{
		let mut cut = best_root.len();
		if full_fixed.as_bytes().get(cut) == Some(&b'\\')
		{
			cut += 1;
		}
		if cut < full_fixed.len()
		{
			return full_fixed[cut..].to_string();
		}
	}

	full_fixed
}

unsafe fn ui_tree_insert(parent: i64, text: &str) -> i64 {
	let ws = to_wstring(text);
	let mut item: TVITEMW = zeroed();
	item.mask = TVIF_TEXT;
	item.pszText = ws.as_ptr();

	let mut ins: TVINSERTSTRUCTW = zeroed();
	ins.hParent = parent;
	ins.hInsertAfter = TVI_LAST;
	ins.item = item;

	SendMessageW(UI_HTREE, TVM_INSERTITEMW, 0, &ins as *const _ as i64)
}

unsafe fn ui_tree_is_dummy(hitem: i64) -> bool {
	if hitem == 0
	{
		return false;
	}
	ui_tree_item_text(hitem).is_empty()
}

unsafe fn ui_tree_add_dummy_child(parent: i64) {
	if parent == 0
	{
		return;
	}
	// Insert an empty-text child so the parent shows the "+" expand button.
	ui_tree_insert(parent, "");
}

unsafe fn ui_tree_load_children(parent: i64, rel_dir: &str) {
	if parent == 0 || UI_HTREE == 0 || MUSIC_DB_PATH.is_empty()
	{
		return;
	}

	let first_child = ui_tree_item_child(parent);
	if first_child == 0 || !ui_tree_is_dummy(first_child)
	{
		// Already loaded (or not a folder).
		return;
	}

	let roots: Vec<(String, String)> = g_root_dir
		.split('|')
		.map(fix_scan_root)
		.filter(|r| !r.is_empty())
		.map(|r| {
			let norm = normalize_path_key(&r);
			(r, norm)
		})
		.collect();

	let Some(hdb) = music_db_open_for_query()
	else
	{
		return;
	};

	let mut folders: HashMap<String, String> = HashMap::default();
	let mut files: HashMap<String, String> = HashMap::default();

	let rel_prefix = if rel_dir.is_empty()
	{
		String::new()
	}
	else
	{
		let mut s = String::with_capacity(rel_dir.len() + 1);
		s.push_str(rel_dir);
		s.push('\\');
		s
	};

	// Query: all songs under abs_dir (dir == abs_dir OR dir starts with abs_dir + '\\').
	let sql = b"SELECT dir, name FROM songs WHERE lower(dir) = ? OR instr(lower(dir), ?) = 1 ORDER BY dir, name;\0";

	for (root, _) in roots.iter()
	{
		let abs_dir = ui_join_root_rel(root, rel_dir);
		let base_norm = normalize_path_key(&abs_dir);
		let mut prefix_norm = base_norm.clone();
		if !prefix_norm.ends_with('\\')
		{
			prefix_norm.push('\\');
		}

		let mut stmt: i64 = 0;
		if sqlite3_prepare_v2(hdb, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
		{
			continue;
		}
		sqlite3_bind_text(stmt, 1, base_norm.as_ptr(), base_norm.len(), -1);
		sqlite3_bind_text(stmt, 2, prefix_norm.as_ptr(), prefix_norm.len(), -1);

		while sqlite3_step(stmt) == SQLITE_ROW
		{
			let dir = sqlite_column_string_raw(stmt, 0);
			let name = sqlite_column_string_raw(stmt, 1);
			if dir.is_empty() || name.is_empty()
			{
				continue;
			}

			let rel = music_tree_relative_path(&roots, &dir, &name);
			if !rel_prefix.is_empty()
			{
				if !rel.starts_with(rel_prefix.as_str())
				{
					continue;
				}
			}
			let rest = if rel_prefix.is_empty() { rel.as_str() } else { &rel[rel_prefix.len()..] };
			if rest.is_empty()
			{
				continue;
			}

			if let Some((child, _tail)) = rest.split_once('\\')
			{
				let k = child.to_ascii_lowercase();
				if !folders.contains_key(&k)
				{
					folders.insert(k, child.to_string());
				}
			}
			else
			{
				let k = rest.to_ascii_lowercase();
				if !files.contains_key(&k)
				{
					files.insert(k, rest.to_string());
				}
			}
		}
		sqlite3_finalize(stmt);
	}

	sqlite3_close(hdb);

	if folders.is_empty() && files.is_empty()
	{
		return;
	}

	// Remove dummy placeholder and insert real children in a single update.
	SendMessageW(UI_HTREE, WM_SETREDRAW, 0, 0);
	SendMessageW(UI_HTREE, TVM_DELETEITEM, 0, first_child);

	let mut folder_names: Vec<String> = folders.into_values().collect();
	folder_names.sort_by(|a, b| cmp_ascii_case_insensitive(a, b).then(a.cmp(b)));

	let mut file_names: Vec<String> = files.into_values().collect();
	file_names.sort_by(|a, b| cmp_ascii_case_insensitive(a, b).then(a.cmp(b)));

	// Insert folders first, then files.
	for name in folder_names
	{
		let h = ui_tree_insert(parent, name.as_str());
		if h != 0
		{
			ui_tree_add_dummy_child(h);
		}
	}
	for name in file_names
	{
		ui_tree_insert(parent, name.as_str());
	}

	SendMessageW(UI_HTREE, WM_SETREDRAW, 1, 0);
	InvalidateRect(UI_HTREE, null(), 1);
}

unsafe fn ui_tree_on_item_expanding(hitem: i64) {
	if hitem == 0 || UI_HTREE == 0
	{
		return;
	}

	let first_child = ui_tree_item_child(hitem);
	if first_child == 0 || !ui_tree_is_dummy(first_child)
	{
		return;
	}

	let parts = ui_tree_item_path_parts(hitem);
	if parts.is_empty()
	{
		return;
	}
	let rel_dir = parts.join("\\");
	ui_tree_load_children(hitem, rel_dir.as_str());
}

unsafe fn ui_music_tree_rebuild() {
	if UI_HTREE == 0 || MUSIC_DB_PATH.is_empty()
	{
		return;
	}

	SendMessageW(UI_HTREE, WM_SETREDRAW, 0, 0);
	SendMessageW(UI_HTREE, TVM_DELETEITEM, 0, TVI_ROOT);

	let roots: Vec<(String, String)> = g_root_dir
		.split('|')
		.map(fix_scan_root)
		.filter(|r| !r.is_empty())
		.map(|r| {
			let norm = normalize_path_key(&r);
			(r, norm)
		})
		.collect();

	let mut hdb: i64 = 0;
	if sqlite3_open16(to_wstring(MUSIC_DB_PATH).as_ptr(), &mut hdb) != SQLITE_OK || hdb == 0
	{
		SendMessageW(UI_HTREE, WM_SETREDRAW, 1, 0);
		return;
	}

	// 避免写锁导致的短暂失败
	sqlite3_exec(hdb, b"PRAGMA busy_timeout=200;\0".as_ptr(), None, null_mut(), null_mut());

	let sql = b"SELECT dir, name FROM songs ORDER BY dir, name;\0";
	let mut stmt: i64 = 0;
	if sqlite3_prepare_v2(hdb, sql.as_ptr(), sql.len(), &mut stmt, 0) != SQLITE_OK
	{
		sqlite3_close(hdb);
		SendMessageW(UI_HTREE, WM_SETREDRAW, 1, 0);
		return;
	}

	// Lazy TreeView: only build root-level items here.
	let mut top_dirs: HashMap<String, String> = HashMap::default();
	let mut top_files: HashMap<String, String> = HashMap::default();
	while sqlite3_step(stmt) == SQLITE_ROW
	{
		let dir = sqlite_column_string_raw(stmt, 0);
		let name = sqlite_column_string_raw(stmt, 1);
		if dir.is_empty() || name.is_empty()
		{
			continue;
		}

		let rel = music_tree_relative_path(&roots, &dir, &name);
		let mut parts = rel
			.split('\\')
			.filter(|s| !s.is_empty());
		let Some(first) = parts.next()
		else
		{
			continue;
		};
		let has_more = parts.next().is_some();
		if has_more
		{
			let key = first.to_ascii_lowercase();
			if !top_dirs.contains_key(&key)
			{
				top_dirs.insert(key, first.to_string());
			}
		}
		else
		{
			let key = first.to_ascii_lowercase();
			if !top_files.contains_key(&key)
			{
				top_files.insert(key, first.to_string());
			}
		}
	}

	sqlite3_finalize(stmt);
	sqlite3_close(hdb);

	let mut dirs: Vec<String> = top_dirs.into_values().collect();
	dirs.sort_by(|a, b| cmp_ascii_case_insensitive(a, b).then(a.cmp(b)));
	for d in dirs
	{
		let h = ui_tree_insert(TVI_ROOT, d.as_str());
		if h != 0
		{
			ui_tree_add_dummy_child(h);
		}
	}

	let mut files: Vec<String> = top_files.into_values().collect();
	files.sort_by(|a, b| cmp_ascii_case_insensitive(a, b).then(a.cmp(b)));
	for f in files
	{
		ui_tree_insert(TVI_ROOT, f.as_str());
	}

	SendMessageW(UI_HTREE, WM_SETREDRAW, 1, 0);
	InvalidateRect(UI_HTREE, null(), 1);
}

// src\var.rs
// 播放状态 (原子变量，线程安全)
static g_track: AtomicUsize = AtomicUsize::new(0);
static g_to_seek: AtomicI32 = AtomicI32::new(0); // 正数快进，负数快退（秒）
static g_to_next: AtomicBool = AtomicBool::new(false);
static g_to_prev: AtomicBool = AtomicBool::new(false);
static g_to_volume: AtomicU32 = AtomicU32::new(100); // 音量百分比 0-100
static g_device_change_tick: AtomicU32 = AtomicU32::new(0); // 上次默认输出设备变更的时间戳 (GetTickCount)
static g_to_pos_ms: AtomicU32 = AtomicU32::new(0); // 模式切换时的恢复位置（毫秒）
static g_is_exclusive: AtomicBool = AtomicBool::new(false); // 当前实际是否在独占输出
static g_li_id: AtomicUsize = AtomicUsize::new(0); // 当前播放列表 ID（li_id），0=无
static g_pl_is_changed: AtomicBool = AtomicBool::new(false); // 播放列表切换触发重新加载
static g_pl_mode: AtomicUsize = AtomicUsize::new(2); // 0=单曲,1=随机,2=顺序(默认)
const RANDOM_METHOD_MEMORY: usize = 0;
const RANDOM_METHOD_SQL: usize = 1;
static g_random_method: AtomicUsize = AtomicUsize::new(RANDOM_METHOD_SQL); // 0=内存随机,1=SQL随机(默认)
const RNG_DEFAULT_SEED: u64 = 88172645463325252;
static RNG_STATE: AtomicU64 = AtomicU64::new(RNG_DEFAULT_SEED);

// fog.db playlists
// NOTE: 0 is reserved for "none" / invalid sentinel.
const PLAYLIST_ID_USER: usize = 1;
const PLAYLIST_ID_DEFAULT: usize = 2;
const PLAYLIST_NAME_USER: &str = "用户";
const PLAYLIST_NAME_DEFAULT: &str = "默认";
static mut g_buffer_duration_hns: i64 = 200_000i64;
static mut g_resume_fade_ms: u32 = 10; // 非 0 起播（恢复/跳到中间）时淡入时长（毫秒）
static mut g_short_fade_ms: u32 = 15; // 曲首/Seek/EOS 短淡入/淡出时长（毫秒）
static m_pl_pool: LazyLock<RwLock<HashMap<usize, Vec<SongInfo>>>> = LazyLock::new(|| RwLock::new(HashMap::default()));

static v_cmd_queue: LazyLock<RwLock<Vec<PlayerCommand>>> = LazyLock::new(|| RwLock::new(Vec::new()));
static is_pl_cmd: AtomicBool = AtomicBool::new(false);
static m_pending_track_by_path: LazyLock<Mutex<HashMap<usize, String>>> = LazyLock::new(|| Mutex::new(HashMap::default()));

static g_pl_state: AtomicU8 = AtomicU8::new(0); // PlayerState::Idle
static g_pending_retry_reason: AtomicU32 = AtomicU32::new(0); // 其他线程发起的重试请求
static LAST_RETRY_REASON: AtomicU32 = AtomicU32::new(0); // 本次 play_track 返回 Some(ms) 的原因

// Ring Buffer 双线程控制变量
const RING_BUFFER_SECONDS: usize = 15; // RingBuffer 缓冲时长（秒）
// RingBuffer 由 main() 统一创建：容量按“最大采样率/声道数”预留，避免每首歌重建
// 384kHz (DXD) 也能覆盖，内存占用约 384000*2*15*8 ≈ 92MB
const RING_BUFFER_MAX_SAMPLE_RATE: usize = 384000;
const RING_BUFFER_MAX_CHANNELS: usize = 2;
const RING_BUFFER_CAPACITY: usize = RING_BUFFER_MAX_SAMPLE_RATE * RING_BUFFER_MAX_CHANNELS * RING_BUFFER_SECONDS;
const EOS_MARKER_BITS: u64 = 0x7ff8_0000_0000_0001;
const EOS_MARKER: f64 = f64::from_bits(EOS_MARKER_BITS);
static g_dec_stop: AtomicBool = AtomicBool::new(false); // 停止解码线程
static g_seek_req_ms: AtomicI64 = AtomicI64::new(-1); // UI 绝对 Seek 请求（毫秒），由 playback 线程平滑处理并提交给解码线程
static g_seek_to_ms: AtomicI64 = AtomicI64::new(-1); // Seek 请求位置（-1=无请求）
static g_seek_just: AtomicBool = AtomicBool::new(false); // 解码线程刚完成 seek（用于清空缓冲）

static SAMPLES_PLAYED: AtomicU64 = AtomicU64::new(0); // 已播放样本数（用于计算位置）
static TRACK_DURATION_MS: AtomicU64 = AtomicU64::new(0); // 当前音轨总时长（毫秒）
static OUTPUT_SAMPLE_RATE: AtomicUsize = AtomicUsize::new(44100); // 输出采样率
static OUTPUT_CHANNELS: AtomicUsize = AtomicUsize::new(2); // 输出声道数
static mut g_ev_ring_space: i64 = 0; // 事件：RingBuffer 有空间（playback -> decode 通知）
static mut g_ev_resume: i64 = 0; // 事件：恢复播放（暂停结束通知）
static mut g_ev_dec_wakeup: i64 = 0; // 事件：唤醒解码线程（Seek 请求时）
static mut g_ev_dec_idle: i64 = 0; // 事件：解码线程空闲/任务结束（manual-reset）
static mut g_ev_pl_quit: i64 = 0; // 事件：通知播放线程结束等待，并退出当前播放任务。g_ev_resume结束等待后继续播放，它是结束后退出当前播放任务。
static mut g_ev_app_quit: i64 = 0; // 事件：全局程序退出信号
static mut g_ev_li_chang: i64 = 0; // 事件：播放列表变更/可用（唤醒等待线程）
static mut G_HOOK_KBD: i64 = 0; // 低级键盘钩子（媒体键）
static mut g_mac_hd: i64 = 0;

static mut G_HWND: i64 = 0;

// UI 窗口模块 - 进度条、播放列表和日志显示

const PROGRESS_NOTIFY_INTERVAL_MS: u64 = 150;

// 窗口相关常量
const WS_OVERLAPPEDWINDOW: u32 = 0x00CF0000;
const WS_VISIBLE: u32 = 0x10000000;
const WS_CHILD: u32 = 0x40000000;
const BS_FLAT: u32 = 0x00008000;
const SS_NOTIFY: u32 = 0x0100;
const SS_GRAYRECT: u32 = 0x00000005;
const SS_OWNERDRAW: u32 = 0x0000000D;
const WS_VSCROLL: u32 = 0x00200000;
const WS_HSCROLL: u32 = 0x00100000;
const WS_BORDER: u32 = 0x00800000;
const WS_CLIPSIBLINGS: u32 = 0x04000000;
const CS_HREDRAW: u32 = 0x0002;
const CS_VREDRAW: u32 = 0x0001;
const CW_USEDEFAULT: i32 = 0x80000000u32 as i32;
const SW_SHOW: i32 = 5;
const SW_SHOWDEFAULT: i32 = 10;
const IDC_ARROW: *const u16 = 32512 as *const u16;
const IDC_SIZEWE: *const u16 = 32644 as *const u16;
const IDC_SIZENS: *const u16 = 32645 as *const u16;
const COLOR_WINDOW: i64 = 5;

// ListView 样式
const LVS_REPORT: u32 = 0x0001;
const LVS_SINGLESEL: u32 = 0x0004;
const LVS_SHOWSELALWAYS: u32 = 0x0008;
const LVS_OWNERDATA: u32 = 0x1000;
const LVS_EX_FULLROWSELECT: u32 = 0x00000020;
const LVS_EX_DOUBLEBUFFER: u32 = 0x00010000;

// Edit 样式
const ES_LEFT: u32 = 0x0000;
const ES_MULTILINE: u32 = 0x0004;
const ES_AUTOVSCROLL: u32 = 0x0040;
const ES_READONLY: u32 = 0x0800;

// 消息常量
const WM_SIZE: u32 = 0x0005;
const WM_CLOSE: u32 = 0x0010;
const WM_SETFONT: u32 = 0x0030;
const WM_CREATE: u32 = 0x0001;
const WM_PAINT: u32 = 0x000F;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_SETREDRAW: u32 = 0x000B;
const WM_HSCROLL: u32 = 0x0114;
const WM_PARENTNOTIFY: u32 = 0x0210;
const WM_SETCURSOR: u32 = 0x0020;
const WM_DRAWITEM: u32 = 0x002B;
// ListView 消息常量
const LVM_FIRST: u32 = 0x1000;
const LVM_GETITEMCOUNT: u32 = LVM_FIRST + 4;
const LVM_SETITEMCOUNT: u32 = LVM_FIRST + 47;
const LVM_INSERTCOLUMNW: u32 = LVM_FIRST + 97;
const LVM_DELETEALLITEMS: u32 = LVM_FIRST + 9;
const LVM_INSERTITEMW: u32 = LVM_FIRST + 77;
const LVM_SETITEMW: u32 = LVM_FIRST + 76;
const LVM_SETITEMSTATE: u32 = LVM_FIRST + 43;
const LVM_GETSELECTEDCOUNT: u32 = LVM_FIRST + 50;
const LVM_GETNEXTITEM: u32 = LVM_FIRST + 12;
const LVM_REDRAWITEMS: u32 = LVM_FIRST + 21;
const LVM_SETEXTENDEDLISTVIEWSTYLE: u32 = LVM_FIRST + 54;
const LVM_GETCOLUMNWIDTH: u32 = LVM_FIRST + 29;
const LVM_SETCOLUMNWIDTH: u32 = LVM_FIRST + 30;
const LVM_GETHEADER: u32 = LVM_FIRST + 31;
const LVM_HITTEST: u32 = LVM_FIRST + 18;
const LVM_GETSTRINGWIDTHW: u32 = LVM_FIRST + 87;
const LVSICF_NOINVALIDATEALL: u32 = 0x00000001;
const LVSICF_NOSCROLL: u32 = 0x00000002;
const LVNI_SELECTED: u32 = 0x0002;
const LVIS_SELECTED: u32 = 0x0002;
const LVIS_FOCUSED: u32 = 0x0001;
const LVIF_TEXT: u32 = 0x0001;
const LVIF_STATE: u32 = 0x0008;
const LVCF_FMT: u32 = 0x0001;
const LVCF_WIDTH: u32 = 0x0002;
const LVCF_TEXT: u32 = 0x0004;
const LVCFMT_LEFT: i32 = 0;
const LVCFMT_RIGHT: i32 = 1;
const LVCFMT_CENTER: i32 = 2;
const LVSCW_AUTOSIZE: i64 = -1;
const LVSCW_AUTOSIZE_USEHEADER: i64 = -2;
const EM_GETSEL: u32 = 0x00B0;
const EM_SETSEL: u32 = 0x00B1;
const EM_REPLACESEL: u32 = 0x00C2;
const EM_SCROLLCARET: u32 = 0x00B7;
const EM_SETLIMITTEXT: u32 = 0x00C5;
const EM_GETLINECOUNT: u32 = 0x00BA;
const EM_LINEINDEX: u32 = 0x00BB;
const LVN_FIRST: i32 = -100;
const LVN_COLUMNCLICK: i32 = LVN_FIRST - 8;
const LVN_GETDISPINFOW: i32 = LVN_FIRST - 77;
const HDN_FIRST: i32 = -300;
const HDN_ENDTRACKA: i32 = HDN_FIRST - 7;
const HDN_ENDTRACKW: i32 = HDN_FIRST - 27;
// Header control messages/flags
const HDM_FIRST: u32 = 0x1200;
const HDM_HITTEST: u32 = HDM_FIRST + 6;
const HHT_ONHEADER: u32 = 0x0002;
const HHT_ONDIVIDER: u32 = 0x0004;
const HHT_ONDIVOPEN: u32 = 0x0008;

// TreeView 消息/样式常量
const TV_FIRST: u32 = 0x1100;
const TVM_DELETEITEM: u32 = TV_FIRST + 1;
const TVM_GETNEXTITEM: u32 = TV_FIRST + 10;
const TVM_SELECTITEM: u32 = TV_FIRST + 11;
const TVM_HITTEST: u32 = TV_FIRST + 17;
const TVM_SETEXTENDEDSTYLE: u32 = TV_FIRST + 44;
const TVM_INSERTITEMW: u32 = TV_FIRST + 50;
const TVM_GETITEMW: u32 = TV_FIRST + 62;
const TVIF_TEXT: u32 = 0x0001;
const TVN_FIRST: i32 = -400;
const TVN_ITEMEXPANDINGW: i32 = TVN_FIRST - 54;
const TVE_EXPAND: u32 = 0x0002;
const TVGN_PARENT: u32 = 0x0003;
const TVGN_CHILD: u32 = 0x0004;
const TVGN_CARET: u32 = 0x0009;
const TVHT_ONITEM: u32 = 0x0046;
const TVS_HASBUTTONS: u32 = 0x0001;
const TVS_HASLINES: u32 = 0x0002;
const TVS_LINESATROOT: u32 = 0x0004;
const TVS_SHOWSELALWAYS: u32 = 0x0020;
const TVS_EX_DOUBLEBUFFER: u32 = 0x0004;
const TVI_ROOT: i64 = -0x10000;
const TVI_LAST: i64 = -0x10002;

// Trackbar (音量) 消息/样式常量
const TBM_GETPOS: u32 = 0x0400;
const TBM_SETPOS: u32 = 0x0400 + 5;
const TBM_SETRANGE: u32 = 0x0400 + 6;
const TBS_NOTICKS: u32 = 0x0010;
const TB_ENDTRACK: u32 = 8;
const WM_SETTEXT: u32 = 0x000C;
const WM_GETTEXTLENGTH: u32 = 0x000E;

// TabControl 消息常量
const TCM_FIRST: u32 = 0x1300;
const TCM_INSERTITEMW: u32 = TCM_FIRST + 62;
const TCM_GETCURSEL: u32 = TCM_FIRST + 11;
const TCM_SETCURSEL: u32 = TCM_FIRST + 12;
const TCM_DELETEITEM: u32 = TCM_FIRST + 8;
const TCM_HITTEST: u32 = TCM_FIRST + 13;
const TCIF_TEXT: u32 = 0x0001;

// 进度条消息常量
const PBM_SETRANGE32: u32 = 0x0406; // WM_USER + 6
const PBM_SETPOS: u32 = 0x0402; // WM_USER + 2
const PBM_SETBKCOLOR: u32 = 0x0401; // WM_USER + 1
const PBM_SETBARCOLOR: u32 = 0x0409; // WM_USER + 9
const PBM_GETPOS: u32 = 0x0408; // WM_USER + 8
const PBS_SMOOTH: u32 = 0x01;

// 控件通知
const WM_COMMAND: u32 = 0x0111;
const WM_NOTIFY: u32 = 0x004E;
const NM_DBLCLK: i32 = -3; // ListView 双击通知
const NM_CLICK: i32 = -2;
const NM_CUSTOMDRAW: i32 = -12;
const TCN_SELCHANGE: i32 = -551; // TabControl 选中变化

// Custom draw (comctl32)
const CDRF_DODEFAULT: u32 = 0x00000000;
const CDRF_NEWFONT: u32 = 0x00000002;
const CDRF_NOTIFYITEMDRAW: u32 = 0x00000020;
const CDRF_NOTIFYSUBITEMDRAW: u32 = 0x00000020;

const CDDS_PREPAINT: u32 = 0x00000001;
const CDDS_ITEMPREPAINT: u32 = 0x00010001;
const CDDS_SUBITEM: u32 = 0x00020000;

// 自定义消息 (从其他线程发送到主线程)

const WM_UI_PROGRESS: u32 = 0x0400 + 201; // WM_USER + 201: 进度更新
const WM_UI_PLAYLIST_UPDATE: u32 = 0x0400 + 202; // WM_USER + 202: 播放列表更新
const WM_UI_PLAYLIST_SELECT: u32 = 0x0400 + 203; // WM_USER + 203: 选中曲目
const WM_UI_NOW_PLAYING: u32 = 0x0400 + 204; // WM_USER + 204: 更新窗口标题和高亮项
const WM_UI_TAB_SYNC: u32 = 0x0400 + 205; // WM_USER + 205: 同步选项卡
const WM_UI_TREE_REFRESH: u32 = 40006; // WM_USER + 206: 刷新音乐库 TreeView
const WM_UI_VOLUME_SYNC: u32 = 0x0400 + 207; // WM_USER + 207: 同步音量滑块
const WM_UI_PLAY_STATE: u32 = 0x0400 + 208; // WM_USER + 208: 播放状态同步 (wparam: 1=播放, 0=暂停)
const WM_UI_LOG_FLUSH: u32 = 0x0400 + 209; // WM_USER + 209: 刷新日志控件
const WM_UI_COVER_SET: u32 = 40011; // Tree 右侧封面：设置图片（lparam=Box<Vec<u8>>*）
const WM_UI_COVER_CLEAR: u32 = WM_USER + 210; // Tree 右侧封面：清空图片
const WM_UI_PLAY_MODE_SINGLE: u32 = 40012; // 播放模式：单曲循环

// 控件 ID
const ID_PROGRESS: i64 = 1000;
const ID_LISTVIEW_BASE: i64 = 2000;
const ID_LOG_EDIT: i64 = 1002;
const ID_BTN_RESTART: i64 = 1003;
const ID_BTN_PREV: i64 = 1007;
const ID_BTN_PLAY: i64 = 1008;
const ID_BTN_PAUSE: i64 = 1009;
const ID_BTN_NEXT: i64 = 1010;
const ID_BTN_RANDOM: i64 = 1011;
const ID_TAB: i64 = 1004;
const ID_TREEVIEW: i64 = 1006;
const ID_VOLUME: i64 = 1012;
const ID_SPLIT_LR: i64 = 1013; // 左右竖列分隔条
const ID_SPLIT_LIST_LOG: i64 = 1014; // 列表/日志分隔条
const ID_COVER: i64 = 1015; // Tree 上方封面控件
const ID_SPLIT_COVER_TREE: i64 = 1016; // 封面/Tree 分隔条

// 进度条高度
const TOOLBAR_HEIGHT: i32 = 40;
const PROGRESS_HEIGHT: i32 = 12;
const PROGRESS_HIT_HEIGHT: i32 = TOOLBAR_HEIGHT; // 进度条可点击区域高度
const PROGRESS_TOTAL_HEIGHT: i32 = TOOLBAR_HEIGHT;
const TAB_HEIGHT: i32 = 26;

const UI_PROGRESS_PLAY_BK_COLOR: u32 = 0x00FAE6C8; // COLORREF (0x00BBGGRR)
const UI_PROGRESS_PLAY_BAR_COLOR: u32 = 0x00DC8C50; // COLORREF (0x00BBGGRR)
const UI_PROGRESS_PAUSE_BK_COLOR: u32 = 0x00D2D2D2; // COLORREF (0x00BBGGRR)
const UI_PROGRESS_PAUSE_BAR_COLOR: u32 = 0x00787878; // COLORREF (0x00BBGGRR)
const UI_NOW_PLAYING_BK_COLOR: u32 = 0x00BEF0FF; // COLORREF (0x00BBGGRR)
const UI_NOW_PLAYING_TEXT_COLOR: u32 = 0x00000000; // COLORREF (black)

// 分隔条外观
const UI_SPLITTER_THICKNESS: i32 = 2;
const UI_SPLITTER_COLOR: u32 = 0x00C0C0C0;

// UI 分隔比例（0-1000，千分比）
static UI_SPLIT_LR: AtomicU32 = AtomicU32::new(720); // 左竖列宽度占比
static UI_SPLIT_LIST_LOG: AtomicU32 = AtomicU32::new(650); // 列表高度占比
static UI_SPLIT_COVER_TREE: AtomicU32 = AtomicU32::new(250); // 右侧封面高度占比（默认 2.5:7.5）
const UI_PLAYLIST_COL_COUNT: usize = 5;
const UI_PLAYLIST_COL_RATIO_TOTAL: u32 = 1000;
const UI_PLAYLIST_COL_RATIO_DEFAULTS: [u32; UI_PLAYLIST_COL_COUNT] = [70, 351, 105, 211, 263];
static UI_PLAYLIST_COL_RATIOS: [AtomicU32; UI_PLAYLIST_COL_COUNT] = [
	AtomicU32::new(UI_PLAYLIST_COL_RATIO_DEFAULTS[0]),
	AtomicU32::new(UI_PLAYLIST_COL_RATIO_DEFAULTS[1]),
	AtomicU32::new(UI_PLAYLIST_COL_RATIO_DEFAULTS[2]),
	AtomicU32::new(UI_PLAYLIST_COL_RATIO_DEFAULTS[3]),
	AtomicU32::new(UI_PLAYLIST_COL_RATIO_DEFAULTS[4]),
];

// 播放列表额外 UI 列：info（不入库，仅运行时计算）
const UI_PLAYLIST_COL_COUNT_TOTAL: usize = UI_PLAYLIST_COL_COUNT + 1;
const UI_PLAYLIST_INFO_COL_IDX: usize = UI_PLAYLIST_COL_COUNT; // 最后一列
const UI_PLAYLIST_INFO_COL_SUBITEM: i32 = UI_PLAYLIST_INFO_COL_IDX as i32;
const UI_PLAYLIST_INFO_COL_RATIO_DEFAULT: u32 = 10; // 3% (ratio total = 1000)

// 全局窗口句柄
static mut UI_HWND: i64 = 0;
static mut UI_HPROGRESS: i64 = 0;
static mut UI_HVOL: i64 = 0;
static mut UI_HBTN_RESTART: i64 = 0;
static mut UI_HBTN_PREV: i64 = 0;
static mut UI_HBTN_PLAY: i64 = 0;
static mut UI_HBTN_PAUSE: i64 = 0;
static mut UI_HBTN_NEXT: i64 = 0;
static mut UI_HBTN_RANDOM: i64 = 0;
static mut UI_HTAB: i64 = 0;
static mut UI_HLIST: i64 = 0;
static mut UI_HCOVER: i64 = 0;
static mut UI_HTREE: i64 = 0;
static mut UI_HLOG: i64 = 0;
static mut UI_HSPLIT_LR: i64 = 0;
static mut UI_HSPLIT_LIST_LOG: i64 = 0;
static mut UI_HSPLIT_COVER_TREE: i64 = 0;
static mut UI_HSPLIT_BRUSH: i64 = 0;
static mut UI_TOOLBAR_PLAYING: bool = false; // 当前播放状态
static mut UI_HBRUSH_PROGRESS_PLAY_BG: i64 = 0; // 进度条播放背景
static mut UI_HBRUSH_PROGRESS_PAUSE_BG: i64 = 0; // 进度条暂停背景
static mut UI_HBRUSH_PROGRESS_PLAY_BAR: i64 = 0; // 进度条播放进度颜色
static mut UI_HBRUSH_PROGRESS_PAUSE_BAR: i64 = 0; // 进度条暂停进度颜色
static mut UI_HFONT: i64 = 0;
static mut UI_HFONT_ICON: i64 = 0;
static mut UI_TREE_OLDPROC: i64 = 0; // TreeView original wndproc
static mut UI_TAB_OLDPROC: i64 = 0; // TabControl original wndproc
static mut UI_PROGRESS_OLDPROC: i64 = 0; // 进度条原始窗口过程
static mut UI_COVER_OLDPROC: i64 = 0; // 封面控件原始窗口过程
static mut UI_CURRENT_PLAYING_LI_ID: usize = 0; // 当前播放项所在播放列表
static mut UI_CURRENT_PLAYING_IDX: i32 = -1; // 当前播放项索引

// Windows API 常量

const WM_DESTROY: u32 = 0x0002;
const WM_SETICON: u32 = 0x0080;
const WM_COPYDATA: u32 = 0x004A;
const WM_QUERYENDSESSION: u32 = 0x0011;
const WM_ENDSESSION: u32 = 0x0016;
const WM_USER: u32 = 0x0400;
const WM_APPCOMMAND: u32 = 0x0319;
const WM_CONTEXTMENU: u32 = 0x007B;

const ICON_SMALL: usize = 0;
const ICON_BIG: usize = 1;

// Tray icon callback (NOTIFYICONDATAW::uCallbackMessage)
const WM_TRAYICON: u32 = WM_USER + 100;

// Mouse messages (used by tray callback lParam)
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_LBUTTONDBLCLK: u32 = 0x0203;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_RBUTTONDBLCLK: u32 = 0x0206;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MBUTTONDBLCLK: u32 = 0x0209;

// Shell_NotifyIcon
const NIM_ADD: u32 = 0x0000;
const NIM_MODIFY: u32 = 0x0001;
const NIM_DELETE: u32 = 0x0002;
const NIM_SETVERSION: u32 = 0x0004;

const NIF_MESSAGE: u32 = 0x0001;
const NIF_ICON: u32 = 0x0002;
const NIF_TIP: u32 = 0x0004;
const NIF_SHOWTIP: u32 = 0x00000080;

const NOTIFYICON_VERSION_4: u32 = 4;

const IMAGE_ICON: u32 = 1;
const LR_LOADFROMFILE: u32 = 0x0010;
const LR_DEFAULTSIZE: u32 = 0x0040;

// 自定义控制消息
const WM_PLAY_PAUSE: u32 = WM_USER + 1;
const WM_RESTART: u32 = WM_USER + 2;
const WM_SEEK_FWD: u32 = WM_USER + 3;
const WM_SEEK_BWD: u32 = WM_USER + 4;
const WM_NEXT_TRACK: u32 = WM_USER + 5;
const WM_PREV_TRACK: u32 = WM_USER + 6;
const WM_VOL_UP: u32 = WM_USER + 7;
const WM_VOL_DOWN: u32 = WM_USER + 8;
const WM_TOGGLE_EXCLUSIVE: u32 = WM_USER + 9; // 切换独占/共享模式
const WM_DEVICE_IN_USE: u32 = WM_USER + 10; // 设备被其他独占进程占用
const WM_PROGRESS: u32 = WM_USER + 11; // 进度通知: wparam=当前位置(ms), lparam=总时长(ms)
const WM_PAUSE: u32 = WM_USER + 12;
const WM_RESUME: u32 = WM_USER + 13;
const WM_SMTC_STATUS: u32 = WM_USER + 14; // SMTC 状态同步: wparam=PlayerState(u8)
const WM_RANDOM_NEXT_TRACK: u32 = WM_USER + 15; // 随机播放下一首（忽略播放模式）
const WM_TOGGLE_WINDOW: u32 = WM_USER + 16; // 切换窗口显隐

// WM_APPCOMMAND (GET_APPCOMMAND_LPARAM)
const APPCOMMAND_MEDIA_NEXTTRACK: u32 = 11;
const APPCOMMAND_MEDIA_PREVIOUSTRACK: u32 = 12;
const APPCOMMAND_MEDIA_PLAY_PAUSE: u32 = 14;
const APPCOMMAND_MEDIA_PLAY: u32 = 46;
const APPCOMMAND_MEDIA_PAUSE: u32 = 47;

// Media keys (VK_*)
const VK_MEDIA_NEXT_TRACK: u32 = 0xB0;
const VK_MEDIA_PREV_TRACK: u32 = 0xB1;
const VK_MEDIA_STOP: u32 = 0xB2;
const VK_MEDIA_PLAY_PAUSE: u32 = 0xB3;

// Low-level keyboard hook
const WH_KEYBOARD_LL: i32 = 13;
const WM_KEYDOWN: u32 = 0x0100;
const WM_SYSKEYDOWN: u32 = 0x0104;

const ICC_PROGRESS_CLASS: u32 = 0x00000020;
const ICC_LISTVIEW_CLASSES: u32 = 0x00000001;
const ICC_TAB_CLASSES: u32 = 0x00000008;
const ICC_TREEVIEW_CLASSES: u32 = 0x00000002;
const ICC_BAR_CLASSES: u32 = 0x00000004;

const WM_TIMER: u32 = 0x0113;
const WM_MOVE: u32 = 0x0003;
const ID_SAVE_TIMER: usize = 999;
const GWLP_WNDPROC: i32 = -4;

const FILE_NOTIFY_CHANGE_FILE_NAME: u32 = 0x00000001;
const FILE_NOTIFY_CHANGE_DIR_NAME: u32 = 0x00000002;
const FILE_NOTIFY_CHANGE_ATTRIBUTES: u32 = 0x00000004;
const FILE_NOTIFY_CHANGE_SIZE: u32 = 0x00000008;
const FILE_NOTIFY_CHANGE_LAST_WRITE: u32 = 0x00000010;
const FILE_NOTIFY_CHANGE_SECURITY: u32 = 0x00000100;

const FILE_ACTION_ADDED: u32 = 1;
const FILE_ACTION_REMOVED: u32 = 2;
const FILE_ACTION_MODIFIED: u32 = 3;
const FILE_ACTION_RENAMED_OLD_NAME: u32 = 4;
const FILE_ACTION_RENAMED_NEW_NAME: u32 = 5;

const FILE_LIST_DIRECTORY: u32 = 0x0001;
const FILE_SHARE_READ: u32 = 0x00000001;
const FILE_SHARE_WRITE: u32 = 0x00000002;
const FILE_SHARE_DELETE: u32 = 0x00000004;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x02000000;
const FILE_FLAG_OVERLAPPED: u32 = 0x40000000;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x00000010;
const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFFFFFF;

const MB_OK: u32 = 0x00000000;
const MB_ICONERROR: u32 = 0x00000010;

// UI 窗口类名
const UI_CLASS_NAME: [u16; 11] = [102, 111, 103, 95, 117, 105, 95, 119, 105, 110, 0]; //r"fog_ui_win"
const MAIN_WIN_CLASS: [u16; 13] = [102, 111, 103, 95, 109, 97, 105, 110, 95, 119, 105, 110, 0]; //r"fog_main_win"

// 进度条类名 (PROGRESS_CLASS = "msctls_progress32")
static PROGRESS_CLASS: &[u16] =
	&[0x6D, 0x73, 0x63, 0x74, 0x6C, 0x73, 0x5F, 0x70, 0x72, 0x6F, 0x67, 0x72, 0x65, 0x73, 0x73, 0x33, 0x32, 0x00]; // "msctls_progress32"

