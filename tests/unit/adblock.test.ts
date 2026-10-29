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

  it('installs native adblock interceptor in Windows window builder', () => {
    const windowSrc = fs.readFileSync('src-tauri/src/app/window.rs', 'utf8');
    expect(windowSrc).toMatch(
      /if window_config\.adblock\s*\{\s*crate::app::adblock::install\(&window\)\?;\s*\}/,
    );
  });

  it('injects cosmetic rules on page load in lib.rs', () => {
    const libSrc = fs.readFileSync('src-tauri/src/lib.rs', 'utf8');
    expect(libSrc).toContain(
      'app::adblock::get_adblock_engine().get_cosmetic_css',
    );
    expect(libSrc).toContain('pake-adblock-cosmetic');
  });

  it('has adblock engine implementation in src-tauri/src/app/adblock.rs', () => {
    const adblockSrc = fs.readFileSync('src-tauri/src/app/adblock.rs', 'utf8');
    expect(adblockSrc).toContain('Engine::new_with_list_text');
    expect(adblockSrc).toContain('should_block');
    expect(adblockSrc).toContain('get_cosmetic_css');
  });
});
