import { lstat, readdir } from 'node:fs/promises';
import { dirname, isAbsolute, join, relative, resolve } from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';

const run = promisify(execFile);
export const PROTECTED_ROOT = '/Library/Fin';
export interface ProtectedPaths {
  installationRoot?: string;
  executablePath?: string;
  configPath?: string;
  credentialsPath?: string;
  storePath?: string;
  helperPath?: string;
  policyPath?: string;
}
export interface InstallationCheck {
  path: string;
  ok: boolean;
  detail: string;
}
export interface InstallationReport {
  ok: boolean;
  checks: InstallationCheck[];
  issues: string[];
  inspectedPaths: number;
}
export interface SecurityMetadata {
  uid: number;
  mode: number;
  symbolicLink: boolean;
  directory: boolean;
}

/** Pure permission rule used by the production validator and isolated test fixtures. */
export function metadataIssues(metadata: SecurityMetadata, privateFile = false): string[] {
  const issues: string[] = [];
  if (metadata.uid !== 0) issues.push('must be owned by root');
  if (metadata.symbolicLink) issues.push('symbolic links are not permitted');
  if ((metadata.mode & 0o022) !== 0) issues.push('must not be writable by group or others');
  if (privateFile && (metadata.mode & 0o077) !== 0)
    issues.push('private files must deny group and other access');
  return issues;
}

function within(root: string, path: string): boolean {
  const part = relative(root, path);
  return part === '' || (!part.startsWith('..') && !isAbsolute(part));
}

/** Fail closed before a broker reads credentials or starts a financial write. */
export async function validateProtectedInstallation(
  options: ProtectedPaths = {},
): Promise<InstallationReport> {
  const root = resolve(options.installationRoot ?? PROTECTED_ROOT);
  const paths = {
    runtime: join(root, 'runtime'),
    private: join(root, 'private'),
    executable: resolve(options.executablePath ?? process.execPath),
    config: resolve(options.configPath ?? join(root, 'private/config.json')),
    credentials: resolve(options.credentialsPath ?? join(root, 'private/credentials.env')),
    store: resolve(options.storePath ?? join(root, 'private/operations.sqlite')),
    helper: resolve(options.helperPath ?? join(root, 'runtime/native/fin-touch-id')),
    policy: resolve(options.policyPath ?? join(root, 'private/approval.json')),
  };
  const checks: InstallationCheck[] = [];
  const add = (path: string, issues: string[]) => {
    // Check every dependency, but keep successful nested paths out of CLI diagnostic output.
    if (issues.length === 0 && within(paths.runtime, path) && !Object.values(paths).includes(path))
      return;
    checks.push({
      path,
      ok: issues.length === 0,
      detail: issues.join('; ') || 'root-owned and protected',
    });
  };
  add(
    'process',
    process.platform !== 'darwin'
      ? ['interactive macOS is required']
      : process.getuid?.() !== 0
        ? ['broker must run as root']
        : [],
  );
  if (root !== PROTECTED_ROOT) add(root, ['production installation must use /Library/Fin']);
  for (const path of Object.values(paths))
    if (!within(root, path)) add(path, ['path escapes the protected installation']);

  const visited = new Set<string>();
  const existing = new Set<string>();
  const recursed = new Set<string>();
  async function inspect(
    path: string,
    privateFile = false,
    recursive = false,
    allowMissing = false,
  ): Promise<void> {
    if (visited.has(path) && !privateFile && (!recursive || recursed.has(path))) return;
    visited.add(path);
    try {
      const stat = await lstat(path);
      existing.add(path);
      const issues = metadataIssues(
        {
          uid: stat.uid,
          mode: stat.mode,
          symbolicLink: stat.isSymbolicLink(),
          directory: stat.isDirectory(),
        },
        privateFile,
      );
      if (privateFile && !stat.isFile()) issues.push('private data paths must be regular files');
      if (
        (path === paths.executable || path === paths.helper) &&
        (!stat.isFile() || (stat.mode & 0o111) === 0)
      )
        issues.push('trusted executables must be executable regular files');
      if (stat.isFile() && (stat.mode & 0o6000) !== 0)
        issues.push('setuid/setgid files are not permitted');
      add(path, issues);
      if (recursive && stat.isDirectory() && !stat.isSymbolicLink()) {
        recursed.add(path);
        for (const child of await readdir(path)) await inspect(join(path, child), false, true);
      }
    } catch (error) {
      if (allowMissing && (error as NodeJS.ErrnoException).code === 'ENOENT') return;
      add(path, ['required path is unavailable']);
    }
  }
  // Writable ancestors would allow an agent to replace an otherwise immutable file.
  for (const path of [root, ...Object.values(paths)]) {
    let parent = dirname(path);
    while (true) {
      await inspect(parent);
      if (parent === '/') break;
      parent = dirname(parent);
    }
  }
  await inspect(root);
  await inspect(paths.runtime, false, true);
  await inspect(paths.private);
  for (const path of [paths.config, paths.credentials, paths.policy]) await inspect(path, true);
  // SQLite sidecars share the credential/state boundary. The database is created by root after validation.
  for (const path of [paths.store, `${paths.store}-wal`, `${paths.store}-shm`])
    await inspect(path, true, false, true);
  await inspect(paths.executable);
  await inspect(paths.helper);
  if (process.platform === 'darwin') {
    try {
      // POSIX modes alone miss ACL grants. Reject all extended ACLs in trusted paths.
      const common = {
        timeout: 30_000,
        env: { PATH: '/usr/bin:/bin', LC_ALL: 'C' },
        maxBuffer: 64 * 1024 * 1024,
      };
      const recursive = await run('/bin/ls', ['-leR', paths.runtime], common);
      const ancestors = await run(
        '/bin/ls',
        ['-lde', ...[...existing].filter((path) => !within(paths.runtime, path))],
        common,
      );
      const listing = `${recursive.stdout}\n${ancestors.stdout}`;
      // '@' can take precedence over '+' when a macOS file has both xattrs and an ACL.
      const hasAcl = /^[-dlcbps][rwxStTs-]{9}\+/m.test(listing) || /^\s*\d+:\s/m.test(listing);
      add(
        root,
        hasAcl
          ? ['extended ACLs are not permitted in the protected installation or its ancestors']
          : [],
      );
    } catch {
      add(root, ['ACL protections could not be established']);
    }
    try {
      await run('/usr/bin/codesign', ['--verify', '--strict', paths.helper], {
        timeout: 10_000,
        env: { PATH: '/usr/bin:/bin' },
      });
      const signature = await run(
        '/usr/bin/codesign',
        ['-d', '--verbose=4', '--entitlements', ':-', paths.helper],
        { timeout: 10_000, env: { PATH: '/usr/bin:/bin' } },
      );
      const text = `${signature.stdout}\n${signature.stderr}`;
      const issues = [];
      if (!/flags=.*runtime/.test(text)) issues.push('Touch ID helper must use hardened runtime');
      if (
        /<key>(?:com\.apple\.security\.)?(?:get-task-allow|cs\.[^<]+)<\/key>\s*<true\s*\/>/.test(
          text,
        )
      )
        issues.push('Touch ID helper permits debugging or injection');
      add(paths.helper, issues);
    } catch {
      add(paths.helper, ['valid hardened code signature could not be established']);
    }
    try {
      const { stdout } = await run('/usr/bin/otool', ['-L', paths.executable], {
        timeout: 10_000,
        env: { PATH: '/usr/bin:/bin' },
      });
      const dependencies = stdout
        .split('\n')
        .slice(1)
        .map((line) => line.trim().split(' ')[0])
        .filter(Boolean);
      add(
        paths.executable,
        dependencies.some(
          (path) => !path!.startsWith('/usr/lib/') && !path!.startsWith('/System/Library/'),
        )
          ? ['Node runtime must link only OS-owned system libraries']
          : [],
      );
    } catch {
      add(paths.executable, ['Node runtime dependencies could not be checked']);
    }
  }
  const issues = checks
    .filter((check) => !check.ok)
    .map((check) => `${check.path}: ${check.detail}`);
  return { ok: issues.length === 0, checks, issues, inspectedPaths: visited.size };
}
