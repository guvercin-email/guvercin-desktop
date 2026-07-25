import { createContext, useCallback, useContext, useMemo, useRef, useState } from 'react'

/**
 * Shared bridge between a mounted rich-text ComposeEditor and the formatting
 * toolbar that lives elsewhere in the tree (the app ribbon, or a detached
 * compose window's top bar).
 *
 * Only one ComposeEditor is ever mounted/visible at a time (inactive compose
 * tabs are not rendered), so a single-slot registry is enough: whichever editor
 * mounts last registers its controller, and unmount clears it.
 *
 * `controller` is the imperative command API exposed by ComposeEditor.
 * `snapshot` mirrors the editor's current formatting state so toolbar buttons
 * can render their active/value states and re-render on every selection change.
 */
const ComposeEditorContext = createContext(null)

export function ComposeEditorProvider({ children }) {
    const controllerRef = useRef(null)
    const [controller, setControllerState] = useState(null)
    const [snapshot, setSnapshot] = useState(null)

    const registerController = useCallback((next) => {
        controllerRef.current = next
        setControllerState(next)
        if (!next) setSnapshot(null)
    }, [])

    const unregisterController = useCallback((instance) => {
        // Only clear if the unmounting editor is still the registered one; a
        // fast unmount/mount swap must not wipe the newcomer's controller.
        if (controllerRef.current === instance) {
            controllerRef.current = null
            setControllerState(null)
            setSnapshot(null)
        }
    }, [])

    const value = useMemo(() => ({
        controller,
        snapshot,
        hasEditor: !!controller,
        registerController,
        unregisterController,
        updateSnapshot: setSnapshot,
    }), [controller, snapshot, registerController, unregisterController])

    return (
        <ComposeEditorContext.Provider value={value}>
            {children}
        </ComposeEditorContext.Provider>
    )
}

export function useComposeEditorContext() {
    return useContext(ComposeEditorContext)
}
