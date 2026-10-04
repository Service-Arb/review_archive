//! What a member does: their gmails, what those track, appeals, Telegram channels. The
//! member is who the caller authenticated as; everything is scoped to them in the store.

use jiff::Timestamp;
use review_archive_core::{
	GbpLocation, Rejected, ReviewId, TargetId, check_lang,
	dto::{Board, GmailDto, GmailOverview, LedgerEntry, NewGmail, NewTgChannel, NewTrack, ReinstatementDto, TargetDto, TargetPatch, TgChannelDto, TokensChange, TokensDto},
};
use tg_types::TelegramDestination;

use super::Archive;

impl Archive {
	/// Adds a managing gmail to the member.
	pub async fn add_gmail(&self, member: &str, req: &NewGmail) -> eyre::Result<GmailDto> {
		let gmail = req.gmail.trim().to_lowercase();
		if gmail.is_empty() || gmail.contains(char::is_whitespace) {
			return Err(Rejected::invalid(format!("{:?} is not an address or an alias for one", req.gmail)).into());
		}
		self.store()?.add_gmail(member, &gmail, Timestamp::now()).await
	}

	/// Removes a gmail with its tracks and the channels scoped to it; one with appeals on
	/// record stays.
	pub async fn delete_gmail(&self, member: &str, id: i64) -> eyre::Result<()> {
		self.store()?.delete_gmail(member, id).await
	}

	/// Tracks a place under the member's gmail: the target on that place, language and
	/// source if there is one — enabled, if it was not — else a new one.
	pub async fn track(&self, member: &str, gmail: i64, req: &NewTrack) -> eyre::Result<TargetDto> {
		let store = self.store()?;
		store.check_gmail(Some(member), gmail).await?;
		let lang = req.lang.clone().unwrap_or_else(|| self.inner.defaults.lang.clone());
		check_lang(&lang)?;
		let gbp = req.gbp.as_deref().map(str::parse::<GbpLocation>).transpose()?;
		let (place_id, _) = crate::places::resolve(&self.inner.http, self.inner.secrets.google_maps_key.as_deref(), &req.place).await?;
		let target = match store.find_target(&place_id, &lang, gbp.as_ref()).await? {
			Some(t) if t.enabled => t,
			Some(t) =>
				self.update_target(
					t.id,
					&TargetPatch {
						enabled: Some(true),
						..Default::default()
					},
				)
				.await?,
			None => self.insert_target(&place_id, req.label.clone(), Some(lang), None, gbp, true).await?.target,
		};
		store.track(Some(member), gmail, target.id, Timestamp::now()).await?;
		Ok(target.into())
	}

	/// The operator puts a target under any gmail.
	pub async fn assign(&self, gmail: i64, target: TargetId) -> eyre::Result<()> {
		self.store()?.track(None, gmail, target, Timestamp::now()).await
	}

	/// Stops the member's gmail tracking a target; the target and its archive stay.
	pub async fn untrack(&self, member: &str, gmail: i64, target: TargetId) -> eyre::Result<()> {
		self.store()?.untrack(member, gmail, target).await
	}

	/// Switches the member's gmail on or off: off, its places are scanned only if someone else has them on.
	pub async fn set_gmail_enabled(&self, member: &str, gmail: i64, on: bool) -> eyre::Result<()> {
		self.store()?.set_gmail_enabled(member, gmail, on).await
	}

	/// Switches the member's gmail's track of a place on or off.
	pub async fn set_track_enabled(&self, member: &str, gmail: i64, target: TargetId, on: bool) -> eyre::Result<()> {
		self.store()?.set_track_enabled(member, gmail, target, on).await
	}

	/// The member's gmails and each one's places.
	pub async fn overview(&self, member: &str) -> eyre::Result<Vec<GmailOverview>> {
		self.store()?.overview(member, Timestamp::now(), &self.inner.tokens).await
	}

	/// A tracked place's reviews: snapshotted, removed, reinstating.
	pub async fn board(&self, member: &str, gmail: i64, target: TargetId) -> eyre::Result<Board> {
		self.store()?.board(member, gmail, target).await
	}

	/// Records that the review's reinstatement was asked of Google, now.
	pub async fn reinstate(&self, member: &str, gmail: i64, review: ReviewId) -> eyre::Result<ReinstatementDto> {
		self.store()?.reinstate(member, gmail, review, Timestamp::now()).await
	}

	/// Withdraws the open appeal; it stays on record.
	pub async fn withdraw_reinstatement(&self, member: &str, gmail: i64, review: ReviewId) -> eyre::Result<()> {
		self.store()?.withdraw_reinstatement(member, gmail, review, Timestamp::now()).await
	}

	/// A capture's AVIF, if it shows a review of a place the member tracks.
	pub async fn member_capture_avif(&self, member: &str, sha256: &str) -> eyre::Result<Vec<u8>> {
		if !self.store()?.member_sees_capture(member, sha256).await? {
			return Err(Rejected::not_found("no such capture").into());
		}
		self.capture_avif(sha256).await
	}

	/// Adds a Telegram channel: a destination the archive's bot can post to, and at least
	/// one event.
	pub async fn add_tg_channel(&self, member: &str, ch: &NewTgChannel) -> eyre::Result<TgChannelDto> {
		ch.destination
			.parse::<TelegramDestination>()
			.map_err(|e| Rejected::invalid(format!("destination {:?}: {e} (expected @channel, a -100… chat id, or <group>/<topic>)", ch.destination)))?;
		if ch.events.is_empty() {
			return Err(Rejected::invalid("subscribe the channel to at least one event").into());
		}
		self.store()?.add_tg_channel(member, ch, Timestamp::now()).await
	}

	/// The member's Telegram channels.
	pub async fn tg_channels(&self, member: &str) -> eyre::Result<Vec<TgChannelDto>> {
		self.store()?.tg_channels(member).await
	}

	/// Removes a channel and what it was still owed.
	pub async fn delete_tg_channel(&self, member: &str, id: i64) -> eyre::Result<()> {
		if !self.store()?.delete_tg_channel(member, id).await? {
			return Err(Rejected::not_found(format!("no Telegram channel {id}")).into());
		}
		Ok(())
	}

	/// Posts a line to the channel now; what Telegram refused comes back as [`Rejected`].
	pub async fn test_tg_channel(&self, member: &str, id: i64) -> eyre::Result<()> {
		let ch = self
			.tg_channels(member)
			.await?
			.into_iter()
			.find(|c| c.id == id)
			.ok_or_else(|| Rejected::not_found(format!("no Telegram channel {id}")))?;
		let events: Vec<&str> = ch.events.iter().map(AsRef::as_ref).collect();
		let text = format!("review_archive: this chat gets {} for {member}", events.join(", "));
		Ok(self.inner.webhooks.test_telegram(&ch.destination, text).await?)
	}

	/// The member's tokens, renewed up to now.
	pub async fn tokens(&self, member: &str) -> eyre::Result<TokensDto> {
		let cfg = &self.inner.tokens;
		Ok(TokensDto {
			balance: self.store()?.balance(member, Timestamp::now(), cfg).await?,
			daily: cfg.daily,
			cap: cfg.cap,
		})
	}

	/// The member's ledger, newest first: 200 rows.
	pub async fn ledger(&self, member: &str) -> eyre::Result<Vec<LedgerEntry>> {
		self.store()?.ledger(member, 200).await
	}

	/// Sets or adds to the member's balance, as admin `by`.
	pub async fn change_tokens(&self, member: &str, req: &TokensChange, by: &str) -> eyre::Result<TokensDto> {
		let cfg = &self.inner.tokens;
		Ok(TokensDto {
			balance: self.store()?.change_balance(member, req.change, by, req.note.as_deref(), Timestamp::now(), cfg).await?,
			daily: cfg.daily,
			cap: cfg.cap,
		})
	}
}
