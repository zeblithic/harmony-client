import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import HintBar from '../HintBar.svelte';

describe('HintBar', () => {
  it('shows flat text when visible', () => {
    render(HintBar, {
      props: { flatText: "KU'E", visible: true },
    });
    expect(screen.getByText("KU'E")).toBeTruthy();
  });

  it('hides text when not visible', () => {
    render(HintBar, {
      props: { flatText: "KU'E", visible: false },
    });
    expect(screen.queryByText("KU'E")).toBeNull();
  });

  it('has accessible label', () => {
    render(HintBar, {
      props: { flatText: "KU'E", visible: true },
    });
    expect(screen.getByLabelText('Phonetic hint')).toBeTruthy();
  });

  it('shows placeholder when flatText is empty', () => {
    render(HintBar, {
      props: { flatText: '', visible: true },
    });
    expect(screen.getByText('No active row')).toBeTruthy();
  });
});
