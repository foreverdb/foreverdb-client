import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
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

/** One person sharing the installation: the accounts of one Battle.net account. */
type User = { bnet_id: string; accounts: AccountStatus[]; linked_account: string | null };

const UNSAVED_WARNING_HOURS = 2;
// English texts with a 24-hour clock and day-first dates.
const LOCALE = "en-GB";

function clock(unixSeconds: number) {
  return new Date(unixSeconds * 1000).toLocaleTimeString(LOCALE, { hour: "2-digit", minute: "2-digit" });
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

function usersOf(clients: ClientStatus[]): User[] {
  const users = new Map<string, User>();
  for (const account of clients.flatMap((client) => client.accounts)) {
    const user = users.get(account.bnet_id) ?? { bnet_id: account.bnet_id, accounts: [], linked_account: null };
    user.accounts.push(account);
    user.linked_account ??= account.linked_account;
    users.set(account.bnet_id, user);
  }
  return [...users.values()];
}

function accountHint(accounts: AccountStatus[]) {
  const characters = [...new Set(accounts.flatMap((account) => account.characters))];
  return characters.length > 0 ? characters.slice(0, 4).join(", ") : accounts.map((account) => account.name).join(", ");
}
type AddonRelease = { version: string; name: string; notes: string; published_at: string; html_url: string; asset_name: string; asset_size: number };
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
  return new Date().toLocaleTimeString(LOCALE, { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

function App() {
  const [installation, setInstallation] = useState<Installation | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  // Read by the activity listener, which is not re-subscribed when the selection changes.
  const selectedRef = useRef(selected);
  selectedRef.current = selected;
  // The account whose upload result or error is shown.
  const [uploadAccount, setUploadAccount] = useState<string | null>(null);
  // Read by the activity listener, like selectedRef.
  const uploadAccountRef = useRef(uploadAccount);
  uploadAccountRef.current = uploadAccount;
  const [result, setResult] = useState<UploadResult | null>(null);
  // Server-side progress of the import behind `result`, reported by the Rust poller.
  const [importStatus, setImportStatus] = useState<ImportStatus | null>(null);
  // General errors (detection, settings); upload and addon errors stay in their cards.
  const [error, setError] = useState("");
  const [uploadError, setUploadError] = useState("");
  const [addonError, setAddonError] = useState("");
  // Key of the account being uploaded by hand.
  const [busy, setBusy] = useState<string | null>(null);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [log, setLog] = useState<LogEntry[]>([]);
  const [dialog, setDialog] = useState<{ candidates: ClientCandidate[]; error: string } | null>(null);
  const [release, setRelease] = useState<AddonRelease | null>(null);
  const [releaseError, setReleaseError] = useState("");
  // Outcome of the last check by hand, so a check that finds nothing does not look like no check at all.
  const [checking, setChecking] = useState(false);
  const [checkResult, setCheckResult] = useState("");
  const [installing, setInstalling] = useState(false);
  const [installed, setInstalled] = useState<InstallResult | null>(null);
  // Pairing codes, the user being linked and link errors, per user (bnet_id).
  const [pairingCodes, setPairingCodes] = useState<Record<string, string>>({});
  const [linking, setLinking] = useState<string | null>(null);
  const [linkErrors, setLinkErrors] = useState<Record<string, string>>({});

  const refresh = useCallback(async () => {
    try {
      const found = await invoke<Installation>("detect_installation");
      setInstallation(found);
      setSelected((current) => {
        if (current && found.clients.some((client) => client.id === current)) return current;
        return found.clients[0]?.id ?? null;
      });
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
    setCheckResult("");
    try {
      const { latest, found } = await checkRelease(true);
      const checked = found?.clients.find((entry) => entry.id === selectedRef.current);
      // An available update or a missing addon already shows itself with its button.
      if (latest && checked?.addon_version && !checked.addon_update) {
        setCheckResult(`No update found – v${checked.addon_version} is up to date (latest release: v${latest.version}).`);
      }
    } finally {
      setChecking(false);
    }
  }

  useEffect(() => {
    void checkRelease(false);
  }, [checkRelease]);

  async function installAddon() {
    if (!client) return;
    setInstalling(true);
    setAddonError("");
    setInstalled(null);
    try {
      setInstalled(await invoke<InstallResult>("install_addon", { client: client.id }));
      await refresh();
    } catch (message) {
      setAddonError(String(message));
    } finally {
      setInstalling(false);
    }
  }

  // The Rust watcher reports what it does (game closed, upload started, result).
  useEffect(() => {
    let active = true;
    const unlisten = listen<Activity>("activity", (event) => {
      if (!active) return; // a listener whose effect was already cleaned up
      logSequence += 1;
      const entry: LogEntry = { ...event.payload, at: timestamp(), id: logSequence };
      // newest first, ordered by sequence no matter in which order events are delivered
      setLog((entries) => [entry, ...entries].sort((a, b) => b.id - a.id).slice(0, LOG_LIMIT));
      if (event.payload.result && event.payload.client === selectedRef.current) {
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
      if (event.payload.kind === "update") void checkRelease(false);
      void refresh();
    });
    return () => {
      active = false;
      void unlisten.then((stop) => stop());
    };
  }, [refresh, checkRelease]);

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
      setSelected(added.id);
      await refresh();
    } catch (message) {
      setDialog((current) => (current ? { ...current, error: String(message) } : current));
    }
  }

  async function chooseFolder() {
    const picked = await open({ directory: true, multiple: false, title: "Choose your WoW installation (\"World of Warcraft\") or a client folder (_classic_beta_ or _classic_era_)" });
    if (typeof picked === "string") await addClient(picked);
  }

  async function removeClient(id: string) {
    try {
      setSettings(await invoke<Settings>("remove_client", { id }));
      if (selected === id) setSelected(null);
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
      await refresh();
    } catch (message) {
      setLinkErrors((errors) => ({ ...errors, [user]: String(message) }));
    } finally {
      setLinking(null);
    }
  }

  const client = installation?.clients.find((entry) => entry.id === selected) ?? null;

  /** The polled status, but only while it still belongs to the shown upload. */
  function importFor(upload: UploadResult): ImportStatus | null {
    return importStatus && importStatus.import_id === upload.import_id ? importStatus : null;
  }

  async function upload(account: AccountStatus) {
    if (!client || !account.file_has_data) return;
    setBusy(account.key);
    setUploadAccount(account.key);
    setUploadError("");
    setResult(null);
    setImportStatus(null);
    try {
      setResult(await invoke<UploadResult>("upload", { client: client.id, account: account.key }));
      await refresh();
    } catch (message) {
      setUploadError(String(message));
    } finally {
      setBusy(null);
    }
  }

  function selectClient(id: string) {
    setSelected(id);
    setUploadAccount(null);
    setResult(null);
    setImportStatus(null);
    setUploadError("");
    setAddonError("");
    setInstalled(null);
    setCheckResult("");
  }

  const clients = installation?.clients ?? [];
  const users = usersOf(clients);
  const addonState = client?.addon_update ? "update" : client?.addon_version ? "ok" : "missing";

  return (
    <main className="shell">
      <header className="masthead">
        <span className="wordmark">ForeverDB</span>
        <span className="masthead-tag">Uploader</span>
      </header>

      <section className="hero">
        <p className="eyebrow">ForeverCollect · Desktop uploader</p>
        <h1>Upload your collected data.</h1>
        <p className="lead">
          ForeverCollect records what you see in World of Warcraft. Every time the game writes its data (logout, /reload, exit),
          the uploader sends it to ForeverDB and keeps a copy in its archive.
        </p>
      </section>

      <section className="intro">
        <label className="switch">
          <input type="checkbox" checked={settings?.auto_upload ?? true} onChange={() => settings && void toggleSetting("set_auto_upload", !settings.auto_upload)} disabled={settings === null} />
          <span className="track" />
          <span>Upload automatically whenever WoW writes the file</span>
        </label>
        <label className="switch">
          <input type="checkbox" checked={settings?.autostart ?? false} onChange={() => settings && void toggleSetting("set_autostart", !settings.autostart)} disabled={settings === null} />
          <span className="track" />
          <span>Start with the system, minimized to the tray</span>
        </label>
        <label className="switch">
          <input type="checkbox" checked={settings?.close_to_tray ?? true} onChange={() => settings && void toggleSetting("set_close_to_tray", !settings.close_to_tray)} disabled={settings === null} />
          <span className="track" />
          <span>Closing the window keeps the uploader running in the tray</span>
        </label>
        {users.length === 0 && (
          <p className="account muted">
            Linking to your ForeverDB account becomes possible once WoW has created your account folder (after your first login).
          </p>
        )}
        {users.map((user) => (
          <form key={user.bnet_id} className="account" onSubmit={(event) => void linkAccount(event, user.bnet_id)}>
            <span>
              {users.length > 1 && <><strong>{accountHint(user.accounts)}</strong> · </>}
              {user.linked_account
                ? <>Linked to <strong>{user.linked_account}</strong></>
                : users.length > 1
                  ? "Link these characters to your ForeverDB account to see them in your profile."
                  : "Link this uploader to your ForeverDB account to see your characters in your profile."}
            </span>
            <div className="account-row">
              <input
                value={pairingCodes[user.bnet_id] ?? ""}
                onChange={(event) => setPairingCodes((codes) => ({ ...codes, [user.bnet_id]: event.target.value }))}
                placeholder="Pairing code from your profile"
                aria-label={`Pairing code for ${user.accounts.map((account) => account.name).join(", ")}`}
                autoComplete="off"
                spellCheck={false}
                disabled={linking !== null}
              />
              <button className="secondary" type="submit" disabled={linking !== null || (pairingCodes[user.bnet_id] ?? "").trim() === ""}>
                {linking === user.bnet_id ? "Linking …" : "Link"}
              </button>
            </div>
            {linkErrors[user.bnet_id] && <small className="account-error">{linkErrors[user.bnet_id]}</small>}
          </form>
        ))}
      </section>

      {installation === null && <div className="card"><p className="muted">Looking for World of Warcraft ...</p></div>}

      {installation && installation.wow_dir === null && clients.length === 0 && (
        <div className="notice error">
          <strong>No supported World of Warcraft installation found</strong>
          <span>Searched in:</span>
          <ul className="paths">{installation.searched.map((path) => <li key={path}>{path}</li>)}</ul>
          <small>Set FOREVERDB_WOW_DIR or choose the installation folder.</small>
          <div className="dialog-actions"><button className="secondary" onClick={openAddDialog}>Choose folder …</button></div>
        </div>
      )}

      {installation && (installation.wow_dir !== null || clients.length > 0) && (
        <>
          <section className="card">
            <div className="section-heading">
              <span className="step">01</span>
              <div>
                <h2>Game client</h2>
                <p>World of Warcraft: Forever and Classic Era / Hardcore are supported. Retail and the other Classic versions are not supported yet.</p>
              </div>
              <button className="link" onClick={openAddDialog}>+ Add client</button>
            </div>
            {clients.length === 0 && (
              <p className="muted">No supported client found. Add its _classic_beta_ or _classic_era_ folder or the WoW installation that contains it.</p>
            )}
            {clients.length > 0 && (
              <div className="client-tabs" role={clients.length > 1 ? "tablist" : undefined} aria-label="Game client">
                {clients.length === 1 ? (
                  <div className="client-tab active">
                    <strong>{clients[0].label}</strong>
                    <span>{clients[0].version ?? "Version unknown"}</span>
                  </div>
                ) : (
                  clients.map((entry) => (
                    <button
                      key={entry.id}
                      role="tab"
                      aria-selected={entry.id === selected}
                      className={`client-tab ${entry.id === selected ? "active" : ""}`}
                      onClick={() => selectClient(entry.id)}
                    >
                      <strong>{entry.label}</strong>
                      <span>{entry.version ?? "Version unknown"}</span>
                    </button>
                  ))
                )}
              </div>
            )}
            {client && (
              <p className="muted small-note path-note">
                {client.id}
                {client.custom && <> · <button className="link" onClick={() => removeClient(client.id)}>remove</button></>}
              </p>
            )}
          </section>

          {client && (
            <section className="card">
              <div className="section-heading">
                <span className="step">02</span>
                <div>
                  <h2>ForeverCollect addon</h2>
                  <p>The addon collects the data in the game. Keep it up to date so the server accepts your uploads.</p>
                </div>
                <button className="link" onClick={() => void checkByHand()} disabled={checking}>
                  {checking ? "Checking …" : "Check for updates"}
                </button>
              </div>
              <div className={`addon-status ${addonState}`}>
                <div>
                  <strong>
                    {client.addon_update
                      ? `ForeverCollect v${client.addon_update} is available`
                      : client.addon_version
                        ? `ForeverCollect v${client.addon_version} is installed`
                        : "ForeverCollect is not installed"}
                  </strong>
                  <small>
                    {client.addon_update && client.addon_version && `Installed: v${client.addon_version} · `}
                    {release && (
                      <>
                        Released {new Date(release.published_at).toLocaleDateString(LOCALE)} ·{" "}
                        <button className="link" onClick={() => void openUrl(release.html_url)}>Release notes</button>
                      </>
                    )}
                    {!release && releaseError && <span className="warning">{releaseError}</span>}
                  </small>
                  {checkResult && !client.addon_update && <small>{checkResult}</small>}
                  {installed && installed.running && <small className="warning">Installed – takes effect after your next login or /reload.</small>}
                </div>
                {(client.addon_update || (!client.addon_version && release)) && (
                  <button className="secondary" onClick={installAddon} disabled={installing}>
                    {installing ? "Installing ..." : client.addon_version ? "Update" : "Install"}
                  </button>
                )}
              </div>
              {addonError && <div className="notice error"><strong>Addon installation failed</strong><span>{addonError}</span></div>}
            </section>
          )}

          {client && (
            <section className="card">
              <div className="section-heading">
                <span className="step">03</span>
                <div>
                  <h2>Upload</h2>
                  <p>{settings?.auto_upload ? "New data is uploaded automatically; you can also upload by hand." : "Automatic upload is off; upload by hand."}</p>
                </div>
              </div>
              {client.accounts.length === 0 && (
                <div className="file-status missing">
                  <span className="status-dot" />
                  <div>
                    <strong>No data yet</strong>
                    <small>No account folder found – log in to WoW once.</small>
                  </div>
                </div>
              )}
              {client.running && <p className="warning small-note">WoW is running – new data arrives when you log out or type /reload.</p>}
              {client.running && unsavedHours(client) >= UNSAVED_WARNING_HOURS && (
                <p className="alert small-note">
                  Nothing saved for {Math.floor(unsavedHours(client))} h – a crash would lose everything since the last save. Type <code>/fc save</code> in the game.
                </p>
              )}
              {client.crash_at !== null && (
                <p className="alert small-note">
                  WoW crashed at {clock(client.crash_at)}{savedAt(client) !== null ? `, last saved at ${clock(savedAt(client)!)}` : ""} – the data of that session was not written.
                </p>
              )}
              {client.accounts.map((account) => (
                <div key={account.key} className="account-upload">
                  <div className={`file-status ${account.file_has_data ? "ok" : "missing"}`}>
                    <span className="status-dot" />
                    <div>
                      <strong>
                        {client.accounts.length > 1 && `${account.name}: `}
                        {account.file_has_data
                          ? (account.from_backup ? "Backup with data ready" : "File ready")
                          : account.file_exists ? "No new data since the last upload" : "No data yet"}
                      </strong>
                      {client.accounts.length > 1 && account.characters.length > 0 && <small>{account.characters.join(", ")}</small>}
                      <small>{account.file_path}</small>
                    </div>
                  </div>

                  <button className="primary" onClick={() => void upload(account)} disabled={busy !== null || !account.file_has_data}>
                    {busy === account.key ? "Uploading ..." : client.accounts.length > 1 ? `Upload ${account.name} now` : "Upload now"}<span>→</span>
                  </button>

                  {uploadAccount === account.key && uploadError && <div className="notice error"><strong>Upload not possible</strong><span>{uploadError}</span></div>}
                  {uploadAccount === account.key && result && (
                    <div className={`notice ${importFor(result)?.status === "failed" ? "error" : "success"}`}>
                      <strong>{importTitle(importFor(result))}</strong>
                      <span>{result.import_id ? `Import ID: ${result.import_id}` : "The server accepted the upload."}</span>
                      {importFor(result)?.status === "failed" && <span>{importFor(result)?.error ?? "The server gave no reason."}</span>}
                      <small>{result.deleted ? "The uploaded data was cleared; the next session starts empty and the addon settings stay." : "WoW is still running: the file stays and is checked again on the next write."}</small>
                    </div>
                  )}
                </div>
              ))}
            </section>
          )}
        </>
      )}

      {log.length > 0 && (
        <section className="card activity">
          <div className="section-heading"><div><h2>Activity</h2><p>Uploads and addon checks of this session.</p></div></div>
          <ul>
            {log.map((entry) => (
              <li key={entry.id} className={entry.kind}>
                <span className="time">{entry.at}</span>
                <span>{entry.message}</span>
              </li>
            ))}
          </ul>
        </section>
      )}

      {error && <div className="notice error"><strong>Something went wrong</strong><span>{error}</span></div>}

      {dialog && (
        <div className="backdrop" onClick={() => setDialog(null)}>
          <div className="dialog" role="dialog" aria-modal="true" onClick={(event) => event.stopPropagation()}>
            <div className="section-heading">
              <div>
                <h2>Add client</h2>
                <p>Supported clients of the installations found, or choose a WoW installation ("World of Warcraft") or its _classic_beta_ or _classic_era_ folder yourself.</p>
              </div>
            </div>
            {dialog.candidates.length === 0 && <p className="muted">No further supported clients found in the known installations.</p>}
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
      <footer>ForeverDB Uploader <span>·</span> {settings?.auto_upload ? "Watching for new data" : "Not watching"}</footer>
    </main>
  );
}

export default App;
