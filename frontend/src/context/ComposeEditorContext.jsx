import { createContext, useCallback, useContext, useMemo, useRef, useState } from 'react'

/**
 * Shared bridge between a mounted rich-text ComposeEditor and the formatting
 * toolbar that lives elsewhere in the tree (the app ribbon, or a detached
 * compose window's top bar).
 *
 * The last focused editor owns the ribbon. When that surface closes, the
 * remaining editor becomes active and restores its own formatting state.
 * `controller` is the imperative command API exposed by ComposeEditor.
 * `snapshot` mirrors the editor's current formatting state so toolbar buttons
 * can render their active/value states and re-render on every selection change.
 */
const ComposeEditorContext = createContext(null)

export function ComposeEditorProvider({ children }) {
    const controllerRef = useRef(null)
    const editorsRef = useRef(new Map())
    const [controller, setControllerState] = useState(null)
    const [snapshot, setSnapshot] = useState(null)

    const focusController = useCallback((next) => {
        if (!editorsRef.current.has(next)) return
        controllerRef.current = next
        setControllerState(next)
        setSnapshot(editorsRef.current.get(next))
    }, [])

    const registerController = useCallback((next) => {
        editorsRef.current.set(next, null)
        if (!controllerRef.current) focusController(next)
    }, [focusController])

    const unregisterController = useCallback((instance) => {
        editorsRef.current.delete(instance)
        if (controllerRef.current !== instance) return
        const remaining = editorsRef.current.keys().next().value
        if (remaining) {
            focusController(remaining)
        } else {
            controllerRef.current = null
            setControllerState(null)
            setSnapshot(null)
        }
    }, [focusController])

    const updateSnapshot = useCallback((instance, next) => {
        if (!editorsRef.current.has(instance)) return
        editorsRef.current.set(instance, next)
        if (controllerRef.current === instance) setSnapshot(next)
    }, [])

    const value = useMemo(() => ({
        controller,
        snapshot,
        hasEditor: !!controller,
        registerController,
        unregisterController,
        updateSnapshot,
        focusController,
    }), [controller, snapshot, registerController, unregisterController, updateSnapshot, focusController])

    return (
        <ComposeEditorContext.Provider value={value}>
            {children}
        </ComposeEditorContext.Provider>
    )
}

export function useComposeEditorContext() {
    return useContext(ComposeEditorContext)
}
