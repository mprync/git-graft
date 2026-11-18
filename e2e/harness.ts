/** Each scenario owns a real repository, its sibling worktrees and an isolated home. */
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { join } from 'node:path';

export type ObjectResult = Record<string, unknown>;
export type Scenario = { name: string; run(ctx: Context): void | Promise<void> };
export class Unavailable extends Error {}

export function objectResult(value: unknown): ObjectResult {
  assert(value !== null && typeof value === 'object' && !Array.isArray(value), 'Expected a JSON object');
  return Object.fromEntries(Object.entries(value));
}

export function execute(exe: string, args: string[], cwd: string, env: NodeJS.ProcessEnv) {
  const out = spawnSync(exe, args, { cwd, env, encoding: 'utf8', windowsHide: true,
    timeout: 120_000, maxBuffer: 24 * 1024 * 1024 });
  return {status: out.status, stdout: out.stdout ?? '', stderr: out.stderr ?? '', error: out.error};
}

export class Context {
  readonly bin: string;
  readonly root: string;
  readonly repo: string;
  readonly env: NodeJS.ProcessEnv;
  readonly commands = new Set<string>();
  readonly tools = new Set<string>();
  readonly observations: Record<string, unknown> = {};

  constructor(bin: string) {
    this.bin = bin;
    const parent = process.env.GIT_GRAFT_TEST_VOLUME ?? tmpdir();
    this.root = realpathSync(mkdtempSync(join(parent, 'git-graft-e2e-')));
    this.repo = join(this.root, 'project');
    this.env = {...process.env};
    for (const key of Object.keys(this.env)) {
      if (key.startsWith('GIT_')) delete this.env[key];
    }
    const home = join(this.root, 'home');
    Object.assign(this.env, {
      HOME: home, USERPROFILE: home, XDG_CONFIG_HOME: join(home, '.config'),
      COREPACK_HOME: process.env.COREPACK_HOME ?? join(process.env.XDG_CACHE_HOME ?? process.env.LOCALAPPDATA ?? join(homedir(),'.cache'),'node','corepack'),
      COREPACK_ENABLE_NETWORK: '0', COREPACK_DEFAULT_TO_LATEST: '0',
      RUSTUP_HOME: process.env.RUSTUP_HOME ?? join(homedir(), '.rustup'),
      GIT_CONFIG_GLOBAL: join(home, 'gitconfig'), GIT_CONFIG_NOSYSTEM: '1',
      GIT_TEMPLATE_DIR: join(this.root, 'template'), GIT_TERMINAL_PROMPT: '0',
      GIT_AUTHOR_NAME: 'Graft Fixture', GIT_COMMITTER_NAME: 'Graft Fixture',
      GIT_AUTHOR_EMAIL: 'test@example.invalid', GIT_COMMITTER_EMAIL: 'test@example.invalid',
      npm_config_audit: 'false', npm_config_fund: 'false', npm_config_update_notifier: 'false',
      UV_PYTHON_DOWNLOADS: 'never', NO_COLOR: '1',
    });
    try {
      for (const dir of [this.repo, home, join(this.root,'template')]) mkdirSync(dir);
      writeFileSync(join(home,'gitconfig'),'[core]\n\tautocrlf = false\n');
      this.git(['init','-q','-b','main']);
      writeFileSync(join(this.repo,'.gitignore'),'ignored/\nnode_modules/\ntarget/\n.venv/\n');
      writeFileSync(join(this.repo,'tracked.txt'),'initial\n');
      this.git(['add','.']);
      this.git(['commit','-qm','initial']);
    } catch (error) { this.close(); throw error; }
  }
  close() { rmSync(this.root, {recursive:true, force:true, maxRetries:5, retryDelay:100}); }
  command(exe: string, args: string[], cwd=this.repo) {return execute(exe,args,cwd,this.env);}
  git(args: string[], cwd=this.repo): string {
    const out=this.command('git',args,cwd);
    assert.equal(out.status,0,out.error?.message ?? out.stderr);
    return out.stdout.replace(/\r?\n$/, '');
  }
  cli(args: string[], cwd=this.repo): ObjectResult {
    const out=this.command(this.bin,['--repo',cwd,'--json',...args],cwd);
    assert.equal(out.status,0,out.error?.message ?? (out.stderr || out.stdout));
    const result=objectResult(JSON.parse(out.stdout));
    this.coverCli(args[0]);
    return result;
  }
  fail(args: string[], cwd=this.repo): ObjectResult {
    const out=this.command(this.bin,['--repo',cwd,'--json',...args],cwd);
    assert.ifError(out.error);
    assert.notEqual(out.status,0,'unexpected command success');
    return objectResult(JSON.parse(out.stderr || out.stdout));
  }
  spawn(name: string, extra: string[]=[], cwd=this.repo) {
    const result=this.cli(['spawn',name,...extra],cwd);
    assert(typeof result.lease==='string' && typeof result.worktree_path==='string');
    return {path:result.worktree_path,lease:result.lease,result};
  }
  coverCli(name: string) {this.commands.add(name);}
  coverTool(name: string) {this.tools.add(name);}
  measure(key: string,value: unknown) {this.observations[key]=value;}
  unavailable(reason: string): never {throw new Unavailable(reason);}
}
