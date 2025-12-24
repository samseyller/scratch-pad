# Scratchpad

**Scratchpad** is a lightweight, single-file text editor for Windows, built with **Rust** and **eframe/egui**.  
It focuses on a clean, Notepad-like interface while adding modern quality-of-life features such as persistent settings, powerful Find/Replace, line numbers, configurable fonts, and safe handling of unsaved changes.

Scratchpad is intentionally simple: it aims to be fast, predictable, and easy to trust for everyday text editing.


## Features

### File handling

- New / Open / Save / Save As with native Windows file dialogs
    
- Open files via **drag and drop**
    
- Open files via **command line** and Windows **“Open With”**
    
- Recent files menu (deduplicated, max 10, persisted, missing file cleanup)
    
- Unsaved changes prompt on **all exit paths** (menu and window close)
    

### Editing

- Copy / Cut / Paste / Select All
    
- Undo (`Ctrl+Z`)
    
- Find / Replace:
    
    - Literal or regex search
        
    - Case sensitivity toggle
        
    - Find next / previous / all
        
    - Replace / Replace All
        
    - Highlight all matches
    

### View & UI

- Configurable editor font size (0.5 steps)
    
- Font family selection (monospace or proportional)
    
- Line numbers with correct wrapping alignment
    
- Word wrap toggle
    
- Scrollable editor
    
- UI font locked for consistent layout
    

### Status bar

- File name and dirty indicator
    
- Cursor line and column
    
- Selection length
    
- Line and character counts
    
- File size (KiB)
    
- Line ending type indicator (LF / CRLF)
    

### Text correctness

- Internal line-ending normalization to **LF**
    
- Preserves original line endings (**CRLF**) on save
    
- Safe and predictable cursor and selection behavior
    

### Platform integration

- Native Windows dialogs
    
- Windows subsystem set to `windows` (no extra console window)
    
- Embedded application icon (generated in code)
    
- About dialog with version, build time, and MIT license
    


## Configuration & Data Storage

Scratchpad persists user data in the following directory:

`%UserProfile%\AppData\Roaming\Scratchpad\data`

### Files in this directory

- **`config.ron`**  
    User-editable configuration file containing:
    
    - Font size and font family
        
    - Line numbers toggle
        
    - Word wrap setting
        
    - Recent files list
        
- **`app.ron`**  
    Internal application state used by egui (window size/position, UI state).  
    This file is automatically managed and generally does not need to be edited manually.
    

Deleting either file will cause Scratchpad to recreate it with default values on next launch.


## Build & Run

### Requirements

- Rust (stable)

### Build (release)

`cargo build --release`

The compiled executable will be located at:

`target\release\scratchpad.exe`

### Run during development

`cargo run`


## License

Scratchpad is licensed under the **MIT License**.

The full license text is included in the repository and is also available in the application’s **About** window.


## Status

Scratchpad is under active development.  
Releases follow semantic versioning (`0.x.y`) while features and behavior continue to evolve.


**Development note:**

Scratchpad was developed with the assistance of modern tooling, including AI-based code generation and review, alongside manual design and testing.
