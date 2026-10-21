import { useCallback, useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { openUrl, revealItemInDir } from "@tauri-apps/plugin-opener";
import "./App.css";

/** A WoW account folder of a client; uploaded on its own. */
type AccountStatus = {
  key: string;
  name: string;
  /** The person behind the account; Battle.net licences "<id>#1", "<id>#2" share it. */
  bnet_id: string;
  characters: string[];
  file_path: string;
  file_exists: boolean;
  file_has_data: boolean;
  from_backup: boolean;
  saved_at: number | null;
  /** Last successful upload of this account. */
  uploaded_at: number | null;
  /** Uploader ID (X-ForeverDB-User) of the Battle.net account; the profile lists links by it. */
  uploader_id: string | null;
  linked_account: string | null;
};

type ClientStatus = {
  id: string;
  label: string;
  product: string;
  version: string | null;
  accounts: AccountStatus[];
  running: boolean;
  custom: boolean;
  addon_version: string | null;
  addon_update: string | null;
  running_since: number | null;
  crash_at: number | null;
};

const UNSAVED_WARNING_HOURS = 2;
/** How long the notice of a completed import stays before it collapses; failures stay. */
const IMPORT_NOTICE_SECONDS = 10;
// English texts with a 24-hour clock and day-first dates.
const LOCALE = "en-GB";
const SITE_URL = "https://www.foreverdb.app/en";
const FOREVER_PRODUCTS = ["wowf", "wow_classic_beta"];

function clock(unixSeconds: number) {
  return new Date(unixSeconds * 1000).toLocaleTimeString(LOCALE, { hour: "2-digit", minute: "2-digit" });
}

/** "today at 10:42", "yesterday at 10:42" or "on 12/10/2026". */
function when(unixSeconds: number) {
  const date = new Date(unixSeconds * 1000);
  const days = Math.round((new Date().setHours(0, 0, 0, 0) - new Date(date).setHours(0, 0, 0, 0)) / 86_400_000);
  if (days === 0) return `today at ${clock(unixSeconds)}`;
  if (days === 1) return `yesterday at ${clock(unixSeconds)}`;
  return `on ${date.toLocaleDateString(LOCALE)}`;
}

/** Last time the game wrote the SavedVariables of any of the client's accounts. */
function savedAt(client: ClientStatus) {
  const times = client.accounts.flatMap((account) => (account.saved_at === null ? [] : [account.saved_at]));
  return times.length > 0 ? Math.max(...times) : null;
}

/** Hours the running game has gone without writing its SavedVariables. */
function unsavedHours(client: ClientStatus) {
  if (client.running_since === null) return 0;
  const since = Math.max(client.running_since, savedAt(client) ?? 0);
  return (Date.now() / 1000 - since) / 3600;
}

/** "Character-Realm" split into name and realm; realms may contain dashes, names do not. */
function splitCharacter(character: string) {
  const at = character.indexOf("-");
  return at < 0 ? { name: character, realm: "" } : { name: character.slice(0, at), realm: character.slice(at + 1) };
}

/** The badge of an account card: what happened to its data last. */
function accountBadge(account: AccountStatus): { tone: "success" | "warning" | "destructive"; text: string } {
  const uploadedLatest = account.uploaded_at !== null && (account.saved_at === null || account.uploaded_at >= account.saved_at);
  if (account.uploaded_at !== null && (uploadedLatest || !account.file_has_data)) {
    return { tone: "success", text: `Last uploaded ${when(account.uploaded_at)}` };
  }
  if (account.file_has_data) return { tone: "warning", text: account.from_backup ? "Backup with data ready" : "New data ready" };
  return { tone: "destructive", text: "Waiting for game data" };
}

type AddonRelease = { version: string; name: string; notes: string; published_at: string; html_url: string; asset_name: string; asset_size: number };
type UploaderUpdate = { current: string; latest: { version: string; published_at: string; html_url: string }; available: boolean };
type InstallResult = { version: string; addon_dir: string; running: boolean };
type ClientCandidate = { id: string; label: string; product: string; version: string | null; has_data: boolean };
type Installation = { wow_dir: string | null; installations: string[]; searched: string[]; clients: ClientStatus[] };
type UploadResult = { import_id: string | null; file_path: string; running: boolean; deleted: boolean };
type Settings = { auto_upload: boolean; close_to_tray: boolean; autostart: boolean; extra_clients: string[]; extra_installations: string[]; repository: string };
type ImportStatus = { import_id: string; status: "queued" | "processing" | "completed" | "failed"; error: string | null };
type Activity = {
  client: string;
  /** Key of the account folder the event is about. */
  account: string | null;
  kind: "pending" | "uploaded" | "cleaned" | "processing" | "completed" | "failed" | "error" | "update";
  message: string;
  result: UploadResult | null;
  import: ImportStatus | null;
};
type LogEntry = Activity & { at: string; id: number };

const REFRESH_INTERVAL_MS = 5000;
const LOG_LIMIT = 8;
// Sequence for log entries; module-level so re-subscribing the event listener (React strict
// mode, dependency changes) never restarts it and keys stay unique and monotonic.
let logSequence = 0;

function importTitle(status: ImportStatus | null) {
  switch (status?.status) {
    case "processing": return "Upload succeeded – processing ...";
    case "completed": return "Import completed";
    case "failed": return "Import failed";
    default: return "Upload succeeded – waiting to be processed";
  }
}

function timestamp() {
  return new Date().toLocaleTimeString(LOCALE, { hour: "2-digit", minute: "2-digit" });
}

// Phosphor icons (regular, 256 grid), as in the Paper design.
const ICONS = {
  upload: "M224,144v64a8,8,0,0,1-8,8H40a8,8,0,0,1-8-8V144a8,8,0,0,1,16,0v56H208V144a8,8,0,0,1,16,0ZM93.66,77.66,120,51.31V144a8,8,0,0,0,16,0V51.31l26.34,26.35a8,8,0,0,0,11.32-11.32l-40-40a8,8,0,0,0-11.32,0l-40,40A8,8,0,0,0,93.66,77.66Z",
  users: "M117.25,157.92a60,60,0,1,0-66.5,0A95.83,95.83,0,0,0,3.53,195.63a8,8,0,1,0,13.4,8.74,80,80,0,0,1,134.14,0,8,8,0,0,0,13.4-8.74A95.83,95.83,0,0,0,117.25,157.92ZM40,108a44,44,0,1,1,44,44A44.05,44.05,0,0,1,40,108Zm210.14,98.7a8,8,0,0,1-11.07-2.33A79.83,79.83,0,0,0,172,168a8,8,0,0,1,0-16,44,44,0,1,0-16.34-84.87,8,8,0,1,1-5.94-14.85,60,60,0,0,1,55.53,105.64,95.83,95.83,0,0,1,47.22,37.71A8,8,0,0,1,250.14,206.7Z",
  caret: "M213.66,101.66l-80,80a8,8,0,0,1-11.32,0l-80-80A8,8,0,0,1,53.66,90.34L128,164.69l74.34-74.35a8,8,0,0,1,11.32,11.32Z",
  shield: "M208,40H48A16,16,0,0,0,32,56v58.78c0,89.61,75.82,119.34,91,124.39a15.53,15.53,0,0,0,10,0c15.2-5.05,91-34.78,91-124.39V56A16,16,0,0,0,208,40Z",
  link: "M240,88.23a54.43,54.43,0,0,1-16,37L189.25,160a54.27,54.27,0,0,1-38.63,16h-.05A54.63,54.63,0,0,1,96,119.84a8,8,0,0,1,16,.45A38.62,38.62,0,0,0,150.58,160h0a38.39,38.39,0,0,0,27.31-11.31l34.75-34.75a38.63,38.63,0,0,0-54.63-54.63l-11,11A8,8,0,0,1,135.7,59l11-11A54.65,54.65,0,0,1,224,48,54.86,54.86,0,0,1,240,88.23ZM109,185.66l-11,11A38.41,38.41,0,0,1,70.6,208h0a38.63,38.63,0,0,1-27.29-65.94L78,107.31A38.63,38.63,0,0,1,144,135.71a8,8,0,0,0,16,.45A54.86,54.86,0,0,0,144,96a54.65,54.65,0,0,0-77.27,0L32,130.75A54.62,54.62,0,0,0,70.56,224h0a54.28,54.28,0,0,0,38.64-16l11-11A8,8,0,0,0,109,185.66Z",
  external: "M224,104a8,8,0,0,1-16,0V59.32l-66.33,66.34a8,8,0,0,1-11.32-11.32L196.68,48H152a8,8,0,0,1,0-16h64a8,8,0,0,1,8,8Zm-40,24a8,8,0,0,0-8,8v72H48V80h72a8,8,0,0,0,0-16H48A16,16,0,0,0,32,80V208a16,16,0,0,0,16,16H176a16,16,0,0,0,16-16V136A8,8,0,0,0,184,128Z",
  checkCircle: "M173.66,98.34a8,8,0,0,1,0,11.32l-56,56a8,8,0,0,1-11.32,0l-24-24a8,8,0,0,1,11.32-11.32L112,148.69l50.34-50.35A8,8,0,0,1,173.66,98.34ZM232,128A104,104,0,1,1,128,24,104.11,104.11,0,0,1,232,128Zm-16,0a88,88,0,1,0-88,88A88.1,88.1,0,0,0,216,128Z",
  check: "M229.66,77.66l-128,128a8,8,0,0,1-11.32,0l-56-56a8,8,0,0,1,11.32-11.32L96,188.69,218.34,66.34a8,8,0,0,1,11.32,11.32Z",
  download: "M128,24A104,104,0,1,0,232,128,104.11,104.11,0,0,0,128,24Zm37.66,101.66-32,32a8,8,0,0,1-11.32,0l-32-32a8,8,0,0,1,11.32-11.32L120,132.69V88a8,8,0,0,1,16,0v44.69l18.34-18.35a8,8,0,0,1,11.32,11.32Z",
  folder: "M245,110.64A16,16,0,0,0,232,104H216V88a16,16,0,0,0-16-16H130.67L102.94,51.2a16.14,16.14,0,0,0-9.6-3.2H40A16,16,0,0,0,24,64V208h0a8,8,0,0,0,8,8H211.1a8,8,0,0,0,7.59-5.47l28.49-85.47A16.05,16.05,0,0,0,245,110.64ZM93.34,64,123.2,86.4A8,8,0,0,0,128,88h72v16H69.77a16,16,0,0,0-15.18,10.94L40,158.7V64Z",
};

/** An inline icon in the current text colour. */
function Icon({ path, size = 16, className }: { path: string; size?: number; className?: string }) {
  return (
    <svg className={`icon ${className ?? ""}`} width={size} height={size} viewBox="0 0 256 256" aria-hidden="true">
      <path d={path} fill="currentColor" />
    </svg>
  );
}

/** A link that opens a page of the ForeverDB website. */
function SiteLink({ path, className, children }: { path: string; className?: string; children: ReactNode }) {
  return (
    <button className={className ?? "link"} onClick={() => void openUrl(`${SITE_URL}${path}`)}>
      {children}<Icon path={ICONS.external} size={12} />
    </button>
  );
}

function App() {
  const [installation, setInstallation] = useState<Installation | null>(null);
  // The account whose upload result or error is shown.
  const [uploadAccount, setUploadAccount] = useState<string | null>(null);
  // Read by the activity listener, which is not re-subscribed when it changes.
  const uploadAccountRef = useRef(uploadAccount);
  uploadAccountRef.current = uploadAccount;
  const [result, setResult] = useState<UploadResult | null>(null);
  // Server-side progress of the import behind `result`, reported by the Rust poller.
  const [importStatus, setImportStatus] = useState<ImportStatus | null>(null);
  // General errors (detection, settings); upload and addon errors stay in their cards.
  const [error, setError] = useState("");
  const [uploadError, setUploadError] = useState("");
  // Key of the account being uploaded by hand.
  const [busy, setBusy] = useState<string | null>(null);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [log, setLog] = useState<LogEntry[]>([]);
  const [dialog, setDialog] = useState<{ candidates: ClientCandidate[]; error: string } | null>(null);
  const [release, setRelease] = useState<AddonRelease | null>(null);
  const [releaseError, setReleaseError] = useState("");
  // Outcome of the last check by hand, per client, so a check that finds nothing does not look like no check at all.
  const [checking, setChecking] = useState(false);
  const [checkResults, setCheckResults] = useState<Record<string, string>>({});
  const [uploaderUpdate, setUploaderUpdate] = useState<UploaderUpdate | null>(null);
  const [version, setVersion] = useState("");
  // Outcome of a check by hand in the footer: "" (none), "checking" or the message to show.
  const [uploaderCheck, setUploaderCheck] = useState("");
  // Addon installation, per client id.
  const [installing, setInstalling] = useState<string | null>(null);
  const [installed, setInstalled] = useState<Record<string, InstallResult>>({});
  const [addonErrors, setAddonErrors] = useState<Record<string, string>>({});
  // Pairing codes, the user being linked and link errors, per user (bnet_id).
  const [pairingCodes, setPairingCodes] = useState<Record<string, string>>({});
  const [linking, setLinking] = useState<string | null>(null);
  const [linkErrors, setLinkErrors] = useState<Record<string, string>>({});
  // Linked accounts whose pairing form is open to link again, per user (bnet_id).
  const [relink, setRelink] = useState<Record<string, boolean>>({});
  // Account cards whose characters are shown, by account key.
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  // The activity log starts collapsed; the user opens it.
  const [logOpen, setLogOpen] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const found = await invoke<Installation>("detect_installation");
      setInstallation(found);
      return found;
    } catch (message) {
      setError(String(message));
      return null;
    }
  }, []);

  // Poll so the file and "game running" status follow what happens in WoW.
  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), REFRESH_INTERVAL_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  useEffect(() => {
    invoke<Settings>("get_settings").then(setSettings).catch((message) => setError(String(message)));
  }, []);

  const checkRelease = useCallback(async (force: boolean) => {
    let latest: AddonRelease | null = null;
    try {
      latest = await invoke<AddonRelease>("check_addon_update", { force });
      setRelease(latest);
      setReleaseError("");
    } catch (message) {
      setRelease(null);
      setReleaseError(String(message));
    }
    return { latest, found: await refresh() };
  }, [refresh]);

  async function checkByHand() {
    setChecking(true);
    setCheckResults({});
    try {
      const { latest, found } = await checkRelease(true);
      // An available update or a missing addon already shows itself with its button.
      if (latest && found) {
        setCheckResults(Object.fromEntries(found.clients
          .filter((client) => client.addon_version && !client.addon_update)
          .map((client) => [client.id, `No update found (latest release: v${latest.version})`])));
      }
    } finally {
      setChecking(false);
    }
  }

  useEffect(() => {
    void checkRelease(false);
  }, [checkRelease]);

  const checkUploader = useCallback(async (force: boolean) => {
    const found = await invoke<UploaderUpdate>("check_uploader_update", { force });
    setUploaderUpdate(found);
    return found;
  }, []);

  useEffect(() => {
    getVersion().then(setVersion).catch(() => undefined);
  }, []);

  useEffect(() => {
    // The background check reports its failures in the activity list.
    checkUploader(false).catch(() => undefined);
  }, [checkUploader]);

  async function checkUploaderByHand() {
    setUploaderCheck("checking");
    try {
      const found = await checkUploader(true);
      setUploaderCheck(found.available ? "" : `Up to date (latest: v${found.latest.version})`);
    } catch (message) {
      setUploaderCheck(String(message));
    }
  }

  async function installAddon(client: ClientStatus) {
    setInstalling(client.id);
    setAddonErrors((errors) => ({ ...errors, [client.id]: "" }));
    try {
      const done = await invoke<InstallResult>("install_addon", { client: client.id });
      setInstalled((current) => ({ ...current, [client.id]: done }));
      await refresh();
    } catch (message) {
      setAddonErrors((errors) => ({ ...errors, [client.id]: String(message) }));
    } finally {
      setInstalling(null);
    }
  }

  // A completed import's notice collapses after a while; the badge keeps the upload time.
  const importCompleted = result !== null && importStatus?.import_id === result.import_id && importStatus.status === "completed";
  useEffect(() => {
    if (!importCompleted) return;
    const timer = window.setTimeout(() => {
      setResult(null);
      setImportStatus(null);
    }, IMPORT_NOTICE_SECONDS * 1000);
    return () => window.clearTimeout(timer);
  }, [importCompleted]);

  // The Rust watcher reports what it does (game closed, upload started, result).
  useEffect(() => {
    let active = true;
    const unlisten = listen<Activity>("activity", (event) => {
      if (!active) return; // a listener whose effect was already cleaned up
      logSequence += 1;
      const entry: LogEntry = { ...event.payload, at: timestamp(), id: logSequence };
      // newest first, ordered by sequence no matter in which order events are delivered
      setLog((entries) => [entry, ...entries].sort((a, b) => b.id - a.id).slice(0, LOG_LIMIT));
      if (event.payload.result) {
        setUploadAccount(event.payload.account);
        setUploadError("");
        setResult(event.payload.result);
        setImportStatus(null);
      }
      if (event.payload.import) setImportStatus(event.payload.import);
      // The watcher removes an uploaded file once the game has closed, after the upload result came in.
      if (event.payload.kind === "cleaned" && event.payload.account === uploadAccountRef.current) {
        setResult((current) => current && { ...current, running: false, deleted: true });
      }
      if (event.payload.kind === "update") {
        void checkRelease(false);
        checkUploader(false).catch(() => undefined);
      }
      void refresh();
    });
    return () => {
      active = false;
      void unlisten.then((stop) => stop());
    };
  }, [refresh, checkRelease, checkUploader]);

  async function openAddDialog() {
    try {
      setDialog({ candidates: await invoke<ClientCandidate[]>("list_client_candidates"), error: "" });
    } catch (message) {
      setError(String(message));
    }
  }

  async function addClient(dir: string) {
    try {
      const added = await invoke<{ id: string; settings: Settings }>("add_client", { dir });
      setSettings(added.settings);
      setDialog(null);
      await refresh();
    } catch (message) {
      setDialog((current) => (current ? { ...current, error: String(message) } : current));
    }
  }

  async function chooseFolder() {
    const picked = await open({ directory: true, multiple: false, title: "Choose your WoW installation (\"World of Warcraft\") or a client folder (_forever_, _classic_beta_ or _classic_era_)" });
    if (typeof picked === "string") await addClient(picked);
  }

  async function removeClient(id: string) {
    try {
      setSettings(await invoke<Settings>("remove_client", { id }));
      await refresh();
    } catch (message) {
      setError(String(message));
    }
  }

  async function toggleSetting(command: string, enabled: boolean) {
    try {
      setSettings(await invoke<Settings>(command, { enabled }));
    } catch (message) {
      setError(String(message));
    }
  }

  async function linkAccount(event: FormEvent, user: string) {
    event.preventDefault();
    setLinking(user);
    setLinkErrors((errors) => ({ ...errors, [user]: "" }));
    try {
      await invoke<string>("link_account", { code: pairingCodes[user] ?? "", user });
      setPairingCodes((codes) => ({ ...codes, [user]: "" }));
      setRelink((current) => ({ ...current, [user]: false }));
      await refresh();
    } catch (message) {
      setLinkErrors((errors) => ({ ...errors, [user]: String(message) }));
    } finally {
      setLinking(null);
    }
  }

  /** The polled status, but only while it still belongs to the shown upload. */
  function importFor(upload: UploadResult): ImportStatus | null {
    return importStatus && importStatus.import_id === upload.import_id ? importStatus : null;
  }

  /** Uploads one account; false if it failed, so "Upload all" can go on with the others. */
  async function upload(client: ClientStatus, account: AccountStatus) {
    if (!account.file_has_data) return false;
    setBusy(account.key);
    setUploadAccount(account.key);
    setUploadError("");
    setResult(null);
    setImportStatus(null);
    try {
      setResult(await invoke<UploadResult>("upload", { client: client.id, account: account.key }));
      return true;
    } catch (message) {
      setUploadError(String(message));
      return false;
    } finally {
      setBusy(null);
      await refresh();
    }
  }

  async function uploadAll() {
    for (const { client, account } of entries.filter((entry) => entry.account.file_has_data)) {
      await upload(client, account);
    }
  }

  async function revealClient(client: ClientStatus) {
    try {
      await revealItemInDir(client.id);
    } catch (message) {
      setError(String(message));
    }
  }

  const clients = installation?.clients ?? [];
  const entries = clients.flatMap((client) => client.accounts.map((account) => ({ client, account })));
  // Licences of one Battle.net account share the uploader ID and are linked together.
  const groups = new Map<string, typeof entries>();
  for (const entry of entries) groups.set(entry.account.bnet_id, [...(groups.get(entry.account.bnet_id) ?? []), entry]);
  const accountNames = new Map(entries.map(({ account }) => [account.key, account.name]));
  const readyCount = entries.filter(({ account }) => account.file_has_data).length;
  // Name the client at each account only when accounts come from more than one client.
  const showClient = clients.filter((client) => client.accounts.length > 0).length > 1;
  const found = installation !== null && (installation.wow_dir !== null || clients.length > 0);

  function accountCard(client: ClientStatus, account: AccountStatus) {
    const badge = accountBadge(account);
    const characters = account.characters.map(splitCharacter);
    const open = expanded[account.key] ?? false;
    const neverWritten = !account.file_exists && account.uploaded_at === null;
    return (
      <article key={account.key} className="account-card">
        <div className="account-head">
          <div className="account-main">
            <div className="account-title">
              <span className="mono-label">{account.name}</span>
              {showClient && <span className="account-client">{client.label}</span>}
              <span className={`badge ${badge.tone}`}><span className="dot" />{badge.text}</span>
            </div>
            <div className="account-characters">
              <Icon path={ICONS.users} className="muted-icon" />
              <strong>{characters.length === 0 ? "No characters yet" : `${characters.length} ${characters.length === 1 ? "character" : "characters"}`}</strong>
              {characters.length > 0 && (
                <button className="link toggle" aria-expanded={open} onClick={() => setExpanded((current) => ({ ...current, [account.key]: !open }))}>
                  {open ? "Hide" : "Show"}<Icon path={ICONS.caret} size={12} className={open ? "flipped" : ""} />
                </button>
              )}
            </div>
            {open ? (
              <div className="account-details">
                <ul className="chips">
                  {characters.map((character) => (
                    <li key={`${character.name}-${character.realm}`}><strong>{character.name}</strong>{character.realm && <span>{character.realm}</span>}</li>
                  ))}
                </ul>
                <div className="callout">
                  <Icon path={ICONS.shield} className="faint-icon" />
                  <div>
                    <strong>The Armory is opt-in</strong>
                    <p>Enable Armory uploads for all characters in the ForeverCollect addon. You can then choose which characters are publicly visible in your ForeverDB profile.</p>
                    <p>Type <code className="command">/fc</code> in-game to manage data sharing.</p>
                  </div>
                </div>
              </div>
            ) : neverWritten ? (
              <p className="note">Log into WoW and type /reload to collect your first data.</p>
            ) : (
              <p className="note with-icon">
                <Icon path={ICONS.shield} size={14} />
                Armory data sharing is optional. Manage it in-game with <code className="command">/fc</code>.
              </p>
            )}
            {client.running && <p className="note warning">WoW is running – new data arrives when you log out or type /reload.</p>}
            {client.running && unsavedHours(client) >= UNSAVED_WARNING_HOURS && (
              <p className="note alert">
                Nothing saved for {Math.floor(unsavedHours(client))} h – a crash would lose everything since the last save. Type <code className="command">/fc save</code> in the game.
              </p>
            )}
            {client.crash_at !== null && (
              <p className="note alert">
                WoW crashed at {clock(client.crash_at)}{savedAt(client) !== null ? `, last saved at ${clock(savedAt(client)!)}` : ""} – the data of that session was not written.
              </p>
            )}
          </div>
          <button className="secondary" onClick={() => void upload(client, account)} disabled={busy !== null || !account.file_has_data}>
            {busy === account.key ? "Uploading …" : "Upload now"}
          </button>
        </div>

        {uploadAccount === account.key && uploadError && (
          <div className="notice error in-card"><strong>Upload not possible</strong><span>{uploadError}</span></div>
        )}
        {uploadAccount === account.key && result && (
          <div className={`notice in-card ${importFor(result)?.status === "failed" ? "error" : "success"}`}>
            <strong>{importTitle(importFor(result))}</strong>
            <span>{result.import_id ? `Import ID: ${result.import_id}` : "The server accepted the upload."}</span>
            {importFor(result)?.status === "failed" && <span>{importFor(result)?.error ?? "The server gave no reason."}</span>}
            <small>{result.deleted ? "The uploaded data was cleared; the next session starts empty and the addon settings stay." : "WoW is still running: the file stays and is checked again on the next write."}</small>
          </div>
        )}

      </article>
    );
  }

  /** Steps to redeem a pairing code from the profile for the uploader ID of `user` (bnet_id). */
  function pairingForm(user: string, uploaderTag: ReactNode) {
    const code = pairingCodes[user] ?? "";
    return (
      <form className="account-foot pairing" onSubmit={(event) => void linkAccount(event, user)}>
        <div className="pairing-title">
          <Icon path={ICONS.link} className="primary-icon" />
          <strong>Link this WoW account to ForeverDB</strong>
          <span>— applies to all its licences (#1, #2 …) in every client{uploaderTag}</span>
        </div>
        <ol className="steps">
          <li>
            <span className="step-title"><span className="step-number">1</span>Open your profile</span>
            <SiteLink path="/profile#uploader" className="tint-button">Profile › Uploader</SiteLink>
          </li>
          <li>
            <span className="step-title"><span className="step-number">2</span>Create a pairing code</span>
            <small>Sign in on ForeverDB and copy the code shown there.</small>
          </li>
          <li className="grow">
            <span className="step-title"><span className="step-number">3</span>Paste it here</span>
            <div className="code-row">
              <input
                value={code}
                onChange={(event) => setPairingCodes((codes) => ({ ...codes, [user]: event.target.value }))}
                placeholder="XXXX-XXXX"
                aria-label={`Pairing code for WoW account ${user}`}
                autoComplete="off"
                spellCheck={false}
                disabled={linking !== null}
              />
              <button className="primary" type="submit" disabled={linking !== null || code.trim() === ""}>
                {linking === user ? "Linking …" : "Link"}
              </button>
            </div>
            {linkErrors[user] && <small className="alert">{linkErrors[user]}</small>}
          </li>
        </ol>
      </form>
    );
  }

  /** One Battle.net account: its licence folders of all clients share one uploader ID, so it is linked once. */
  function wowAccountGroup(user: string, members: { client: ClientStatus; account: AccountStatus }[]) {
    const linked = members[0].account.linked_account;
    const relinking = relink[user] ?? false;
    const uploaderId = members[0].account.uploader_id;
    const uploaderTag = uploaderId && <> · uploader <code className="mono-label">{uploaderId.slice(0, 8)}</code></>;
    const licences = members.length;
    const clientCount = new Set(members.map(({ client }) => client.id)).size;
    return (
      <section key={user} className="account-card wow-account">
        <div className="wow-account-head">
          <span className="eyebrow">WoW account</span>
          <span className="mono-label">{user}</span>
          <span className="account-client">
            {licences} {licences === 1 ? "licence" : "licences"}{clientCount > 1 ? ` in ${clientCount} clients` : ""}
          </span>
        </div>
        <div className="wow-account-licences">{members.map(({ client, account }) => accountCard(client, account))}</div>
        {linked ? (
          <>
            <div className="account-foot linked">
              <Icon path={ICONS.checkCircle} className="success-icon" />
              <span className="linked-text">
                Linked to <strong>{linked}</strong>{uploaderTag} · <SiteLink path="/profile#characters">Choose visible characters in your profile</SiteLink>
              </span>
              <button className="link toggle" aria-expanded={relinking} onClick={() => setRelink((current) => ({ ...current, [user]: !relinking }))}>
                Link again<Icon path={ICONS.caret} size={12} className={relinking ? "flipped" : ""} />
              </button>
              <SiteLink path="/profile#uploader" className="link quiet">Unlink</SiteLink>
            </div>
            {relinking && pairingForm(user, uploaderTag)}
          </>
        ) : (
          pairingForm(user, uploaderTag)
        )}
      </section>
    );
  }

  function clientCard(client: ClientStatus) {
    const addonState = client.addon_update ? "update" : client.addon_version ? "ok" : "missing";
    const done = installed[client.id];
    return (
      <article key={client.id} className="client-card">
        <div className="client-head">
          <span className={`client-icon ${FOREVER_PRODUCTS.includes(client.product) ? "forever" : ""}`}><Icon path={ICONS.shield} /></span>
          <div>
            <strong>{client.label}</strong>
            <span className="mono-label small">{client.version ?? "Version unknown"}</span>
          </div>
          {client.custom && <button className="link quiet small" onClick={() => void removeClient(client.id)}>Remove</button>}
        </div>
        <div className="client-path">
          <span title={client.id}>{client.id}</span>
          <button className="icon-button" onClick={() => void revealClient(client)} aria-label={`Open ${client.label} folder`}>
            <Icon path={ICONS.folder} size={12} />
          </button>
        </div>
        <div className={`addon-row ${addonState}`}>
          <Icon path={addonState === "ok" ? ICONS.check : ICONS.download} size={14} className={addonState === "ok" ? "success-icon" : "primary-icon"} />
          <div>
            <strong>{client.addon_version ? `ForeverCollect v${client.addon_version}` : "ForeverCollect not installed"}</strong>
            <small>
              {client.addon_update ? `v${client.addon_update} available` : client.addon_version ? (checkResults[client.id] ?? "Up to date") : release ? `v${release.version} available` : "Not installed"}
              {release && <> · <button className="link inherit" onClick={() => void openUrl(release.html_url)}>Release notes</button></>}
            </small>
            {!release && releaseError && <small className="alert">{releaseError}</small>}
            {done?.running && <small className="warning">Installed – takes effect after your next login or /reload.</small>}
            {addonErrors[client.id] && <small className="alert">{addonErrors[client.id]}</small>}
          </div>
          {client.addon_update || (!client.addon_version && release) ? (
            <button className="primary small" onClick={() => void installAddon(client)} disabled={installing !== null}>
              {installing === client.id ? "Installing …" : client.addon_version ? "Update" : "Install"}
            </button>
          ) : (
            <button className="link" onClick={() => void checkByHand()} disabled={checking}>{checking ? "Checking …" : "Check"}</button>
          )}
        </div>
      </article>
    );
  }

  // The watcher only has something to watch once a client was found.
  const watchStatus = installation === null
    ? { tone: "off", text: "Looking for World of Warcraft …" }
    : clients.length === 0
      ? { tone: "missing", text: "No game client found" }
      : settings?.auto_upload === false
        ? { tone: "off", text: "Automatic upload off" }
        : { tone: "", text: "Watching for changes" };

  const preferences = [
    { command: "set_auto_upload", value: settings?.auto_upload ?? true, title: "Upload automatically", hint: "Whenever WoW writes the file" },
    { command: "set_autostart", value: settings?.autostart ?? false, title: "Launch at startup", hint: "Minimized to the tray" },
    { command: "set_close_to_tray", value: settings?.close_to_tray ?? true, title: "Minimize to system tray", hint: "The uploader stays in the tray" },
  ];

  return (
    <div className="app">
      <header className="masthead">
        <div className="brand">
          <span className="wordmark">ForeverDB</span>
          <span className="divider" />
          <span className="masthead-tag">Uploader</span>
        </div>
        <span className={`status-pill ${watchStatus.tone}`}>
          <span className="dot" />
          {watchStatus.text}
        </span>
      </header>

      {uploaderUpdate?.available && (
        <div className="update-banner">
          <strong>ForeverDB Uploader v{uploaderUpdate.latest.version} is available</strong>
          <span>Installed: v{uploaderUpdate.current}</span>
          <SiteLink path="/info/uploader">Download</SiteLink>
        </div>
      )}

      <div className="layout">
        <main className="main">
          <section className="hero">
            <div>
              <p className="eyebrow">ForeverCollect · {groups.size} {groups.size === 1 ? "WoW account" : "WoW accounts"}</p>
              <h1>Your Game Data</h1>
              <p className="lead">Every time WoW writes its data — logout, /reload or exit — the uploader sends it to ForeverDB and keeps a copy in its archive.</p>
            </div>
            <button className="primary large" onClick={() => void uploadAll()} disabled={busy !== null || readyCount === 0}>
              <Icon path={ICONS.upload} />Upload all now
            </button>
          </section>

          {installation === null && <div className="account-card empty"><p className="note">Looking for World of Warcraft …</p></div>}

          {installation && !found && (
            <div className="notice error">
              <strong>No supported World of Warcraft installation found</strong>
              <span>Searched in:</span>
              <ul className="paths">{installation.searched.map((path) => <li key={path}>{path}</li>)}</ul>
              <small>Set FOREVERDB_WOW_DIR or choose the installation folder.</small>
              <div className="dialog-actions"><button className="secondary" onClick={openAddDialog}>Choose folder …</button></div>
            </div>
          )}

          {found && entries.length === 0 && (
            <div className="account-card empty">
              <p className="note">
                {clients.length === 0
                  ? "No supported client found. Add its _forever_, _classic_beta_ or _classic_era_ folder or the WoW installation that contains it."
                  : "No account folder found yet – log in to WoW once to create it."}
              </p>
            </div>
          )}

          {entries.length > 0 && <div className="accounts">{[...groups].map(([user, members]) => wowAccountGroup(user, members))}</div>}

          {error && <div className="notice error"><strong>Something went wrong</strong><span>{error}</span></div>}

          <section className="recent">
            <div className="section-row">
              <h2 className="section-label">Recent activity{log.length > 0 && ` · ${log.length}`}</h2>
              <button className="link toggle" aria-expanded={logOpen} onClick={() => setLogOpen((open) => !open)}>
                {logOpen ? "Hide" : "Show"}<Icon path={ICONS.caret} size={12} className={logOpen ? "flipped" : ""} />
              </button>
            </div>
            {logOpen && log.length === 0 && <p className="note">No recent activity.</p>}
            {logOpen && log.length > 0 && (
              <ul>
                {log.map((entry) => {
                  const name = entry.account ? accountNames.get(entry.account) : undefined;
                  const message = name && entry.message.startsWith(`${name}: `) ? entry.message.slice(name.length + 2) : entry.message;
                  return (
                    <li key={entry.id} className={entry.kind}>
                      <span className="time">{entry.at}</span>
                      <span className="who">{name ?? ""}</span>
                      <span className="what">{message}</span>
                    </li>
                  );
                })}
              </ul>
            )}
          </section>
        </main>

        <aside className="sidebar">
          <section className="sidebar-section">
            <div className="section-row">
              <h2 className="section-label">Game clients</h2>
              <button className="link" onClick={openAddDialog}>+ Add client</button>
            </div>
            {clients.length > 0 && <div className="clients">{clients.map(clientCard)}</div>}
            <p className="fine-print">Retail and other Classic versions aren't supported yet.</p>
          </section>

          <section className="sidebar-section preferences">
            <h2 className="section-label">Preferences</h2>
            {preferences.map((preference) => (
              <label key={preference.command} className="switch">
                <span>
                  <strong>{preference.title}</strong>
                  <small>{preference.hint}</small>
                </span>
                <input type="checkbox" checked={preference.value} onChange={() => void toggleSetting(preference.command, !preference.value)} disabled={settings === null} />
                <span className="track" />
              </label>
            ))}
          </section>

          <footer className="sidebar-foot">
            <span className="mono-label small">Uploader{version && ` v${version}`}</span>
            {uploaderCheck === "" || uploaderCheck === "checking"
              ? <button className="link" onClick={() => void checkUploaderByHand()} disabled={uploaderCheck === "checking"}>
                  {uploaderCheck === "checking" ? "Checking …" : "Check for updates"}
                </button>
              : <small>{uploaderCheck}</small>}
          </footer>
        </aside>
      </div>

      {dialog && (
        <div className="backdrop" onClick={() => setDialog(null)}>
          <div className="dialog" role="dialog" aria-modal="true" onClick={(event) => event.stopPropagation()}>
            <h2>Add client</h2>
            <p className="note">Supported clients of the installations found, or choose a WoW installation ("World of Warcraft") or its _forever_, _classic_beta_ or _classic_era_ folder yourself.</p>
            {dialog.candidates.length === 0 && <p className="note">No further supported clients found in the known installations.</p>}
            <ul className="candidates">
              {dialog.candidates.map((candidate) => (
                <li key={candidate.id}>
                  <div>
                    <strong>{candidate.label}</strong>
                    <span>{candidate.version ?? "not installed in the launcher"}{candidate.has_data ? " · has data" : ""}</span>
                    <small>{candidate.id}</small>
                  </div>
                  <button className="secondary" onClick={() => addClient(candidate.id)}>Add</button>
                </li>
              ))}
            </ul>
            {dialog.error && <div className="notice error"><span>{dialog.error}</span></div>}
            <div className="dialog-actions">
              <button className="secondary" onClick={chooseFolder}>Choose folder …</button>
              <button className="link" onClick={() => setDialog(null)}>Close</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

export default App;
