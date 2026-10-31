import fs from 'node:fs';
import { describe, expect, it } from 'vitest';
import { getCliProgram } from '../../bin/helpers/cli-program.js';
import { buildWindowConfigOverrides } from '../../bin/helpers/merge.js';
import { DEFAULT_PAKE_OPTIONS } from '../../bin/defaults.js';
import type { PakeAppOptions } from '../../bin/types.js';

function makeOptions(overrides: Partial<PakeAppOptions> = {}): PakeAppOptions {
  return {
    ...DEFAULT_PAKE_OPTIONS,
    identifier: 'com.pake.test',
    ...overrides,
  };
}

describe('Adblock feature integration', () => {
  it('registers --adblock option with default false', () => {
    const program = getCliProgram();
    const option = program.options.find((item) => item.long === '--adblock');

    expect(option).toBeDefined();
    expect(option?.defaultValue).toBe(false);
    expect(program.helpInformation()).toContain('--adblock');
  });

  it.each([true, false])(
    'forwards adblock=%s to native window configuration',
    (enabled) => {
      const overrides = buildWindowConfigOverrides(
        makeOptions({ adblock: enabled }),
        'win32',
      );
      expect(overrides.adblock).toBe(enabled);
    },
  );

  it('registers the WebView2 filter before the first navigation', () => {
    const windowSrc = fs.readFileSync('src-tauri/src/app/window.rs', 'utf8');
    const adblockSrc = fs.readFileSync('src-tauri/src/app/adblock.rs', 'utf8');
    const adblockIndex = windowSrc.indexOf(
      'crate::app::adblock::install(&window)',
    );
    // download::install ends by navigating away from about:blank, so anything
    // registered after it misses the first page load.
    const downloadIndex = windowSrc.indexOf(
      'crate::app::download::install(&window',
    );

    expect(adblockIndex).toBeGreaterThan(-1);
    expect(downloadIndex).toBeGreaterThan(-1);
    expect(adblockIndex).toBeLessThan(downloadIndex);
    expect(adblockSrc).toContain('FrameCreatedEventHandler');
    expect(adblockSrc).toContain('handle_frame_navigation');
    expect(adblockSrc).toContain('"subdocument"');
  });

  it('loads the engine before the first window is built', () => {
    const libSrc = fs.readFileSync('src-tauri/src/lib.rs', 'utf8');
    const initIndex = libSrc.indexOf('app::adblock::init(app.app_handle())');
    const windowIndex = libSrc.indexOf('let window = set_window(');

    expect(initIndex).toBeGreaterThan(-1);
    expect(windowIndex).toBeGreaterThan(-1);
    expect(initIndex).toBeLessThan(windowIndex);
    expect(libSrc).toContain('app::adblock::cosmetic_page_script');
  });

  it('connects Brave generic cosmetic matching to bounded DOM observation', () => {
    const adblockSrc = fs.readFileSync('src-tauri/src/app/adblock.rs', 'utf8');
    const invokeSrc = fs.readFileSync('src-tauri/src/app/invoke.rs', 'utf8');
    const windowSrc = fs.readFileSync('src-tauri/src/app/window.rs', 'utf8');
    const libSrc = fs.readFileSync('src-tauri/src/lib.rs', 'utf8');
    const buildSrc = fs.readFileSync('src-tauri/build.rs', 'utf8');
    const capability = JSON.parse(
      fs.readFileSync('src-tauri/capabilities/default.json', 'utf8'),
    );
    const script = fs.readFileSync(
      'src-tauri/src/inject/adblock_cosmetic.js',
      'utf8',
    );

    expect(adblockSrc).toContain('hidden_class_id_selectors');
    expect(invokeSrc).toContain('validate_cosmetic_tokens');
    expect(invokeSrc).toContain('window.url()');
    expect(windowSrc).toContain('adblock_cosmetic.js');
    expect(libSrc).toContain('adblock_hidden_class_id_selectors');
    expect(buildSrc).toContain('"adblock_hidden_class_id_selectors"');
    expect(capability.permissions).toContain(
      'allow-adblock-hidden-class-id-selectors',
    );
    expect(script).toContain('new MutationObserver');
    expect(script).toContain('MAX_TRACKED_TOKENS');
    expect(script).toContain('adblock_hidden_class_id_selectors');
  });

  it('automatically enables runtime diagnostics when adblock is initialized', () => {
    const adblockSrc = fs.readFileSync('src-tauri/src/app/adblock.rs', 'utf8');
    const docs = fs.readFileSync('docs/cli-usage.md', 'utf8');

    expect(adblockSrc).toContain('dir.join("adblock-debug.log")');
    expect(adblockSrc).toContain(
      'DEBUG_ENABLED.store(true, Ordering::Release)',
    );
    expect(adblockSrc).not.toContain('PAKE_ADBLOCK_DEBUG');
    expect(docs).toContain('automatically writes diagnostics at runtime');
  });

  it('builds the engine from real filter lists and native request filtering', () => {
    const adblockSrc = fs.readFileSync('src-tauri/src/app/adblock.rs', 'utf8');

    // Engine construction goes through FilterSet so multiple lists can be
    // merged before parsing, which is what adblock-rust requires.
    expect(adblockSrc).toContain('FilterSet::new');
    expect(adblockSrc).toContain('Engine::new_with_filter_set');
    expect(adblockSrc).toContain('check_network_request');
    expect(adblockSrc).toContain('url_cosmetic_resources');

    // The published lists, cached under the app config directory.
    expect(adblockSrc).toContain('https://easylist.to/easylist/easylist.txt');
    expect(adblockSrc).toContain(
      'https://easylist.to/easylist/easyprivacy.txt',
    );
    expect(adblockSrc).toContain('app_config_dir');

    // Native blocking.
    expect(adblockSrc).toContain('WebResourceRequestedEventHandler');
    expect(adblockSrc).toContain('AddWebResourceRequestedFilter');
  });

  it('ships a fallback rule list and a generic cosmetic sheet', () => {
    const rules = fs.readFileSync(
      'src-tauri/src/app/adblock_defaults.txt',
      'utf8',
    );
    const cosmetic = fs.readFileSync(
      'src-tauri/src/app/adblock_cosmetic.css',
      'utf8',
    );

    expect(rules).toContain('||doubleclick.net^');
    expect(rules).toContain('||googlesyndication.com^');
    expect(cosmetic).toContain('.adsbygoogle');
    expect(cosmetic).toContain('display: none !important');
  });

  it('depends on the adblock engine and an HTTP client for the lists', () => {
    const cargo = fs.readFileSync('src-tauri/Cargo.toml', 'utf8');

    expect(cargo).toMatch(/^adblock\s*=/m);
    expect(cargo).toMatch(/^reqwest\s*=/m);
  });
});
