import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import HelpMenuButton from '../HelpMenuButton.svelte';

function defaultProps() {
  return {
    onSubmitFeedback: vi.fn(),
    onShowAbout: vi.fn(),
    onOpenNetworkHealth: vi.fn(),
    onOpenDocs: vi.fn(),
  };
}

describe('HelpMenuButton', () => {
  it('renders the (?) button with aria-label', () => {
    render(HelpMenuButton, defaultProps());
    const button = screen.getByTestId('help-menu-button');
    expect(button.getAttribute('aria-label')).toBe('Help and feedback');
  });

  it('dropdown is hidden initially', () => {
    render(HelpMenuButton, defaultProps());
    expect(screen.queryByTestId('help-menu-dropdown')).toBeNull();
  });

  it('click → dropdown opens with 4 items in spec order', async () => {
    render(HelpMenuButton, defaultProps());
    await fireEvent.click(screen.getByTestId('help-menu-button'));
    const items = screen.getAllByRole('menuitem');
    expect(items).toHaveLength(4);
    expect(items[0]).toHaveTextContent(/Submit Feedback/i);
    expect(items[1]).toHaveTextContent(/Network Health/i);
    expect(items[2]).toHaveTextContent(/About/i);
    expect(items[3]).toHaveTextContent(/Documentation/i);
  });

  it('click outside closes dropdown', async () => {
    render(HelpMenuButton, defaultProps());
    await fireEvent.click(screen.getByTestId('help-menu-button'));
    expect(screen.getByTestId('help-menu-dropdown')).toBeInTheDocument();
    // Click on the document body
    await fireEvent.mouseDown(document.body);
    expect(screen.queryByTestId('help-menu-dropdown')).toBeNull();
  });

  it('Escape closes dropdown', async () => {
    render(HelpMenuButton, defaultProps());
    await fireEvent.click(screen.getByTestId('help-menu-button'));
    expect(screen.getByTestId('help-menu-dropdown')).toBeInTheDocument();
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(screen.queryByTestId('help-menu-dropdown')).toBeNull();
  });

  it('Submit Feedback item → onSubmitFeedback + close', async () => {
    const props = defaultProps();
    render(HelpMenuButton, props);
    await fireEvent.click(screen.getByTestId('help-menu-button'));
    await fireEvent.click(screen.getByTestId('help-menu-feedback'));
    expect(props.onSubmitFeedback).toHaveBeenCalled();
    expect(screen.queryByTestId('help-menu-dropdown')).toBeNull();
  });

  it('Network Health item → onOpenNetworkHealth + close', async () => {
    const props = defaultProps();
    render(HelpMenuButton, props);
    await fireEvent.click(screen.getByTestId('help-menu-button'));
    await fireEvent.click(screen.getByTestId('help-menu-network'));
    expect(props.onOpenNetworkHealth).toHaveBeenCalled();
    expect(screen.queryByTestId('help-menu-dropdown')).toBeNull();
  });

  it('About item → onShowAbout + close', async () => {
    const props = defaultProps();
    render(HelpMenuButton, props);
    await fireEvent.click(screen.getByTestId('help-menu-button'));
    await fireEvent.click(screen.getByTestId('help-menu-about'));
    expect(props.onShowAbout).toHaveBeenCalled();
    expect(screen.queryByTestId('help-menu-dropdown')).toBeNull();
  });

  it('Documentation item → onOpenDocs + close', async () => {
    const props = defaultProps();
    render(HelpMenuButton, props);
    await fireEvent.click(screen.getByTestId('help-menu-button'));
    await fireEvent.click(screen.getByTestId('help-menu-docs'));
    expect(props.onOpenDocs).toHaveBeenCalled();
    expect(screen.queryByTestId('help-menu-dropdown')).toBeNull();
  });
});
