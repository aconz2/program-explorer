import { render, createRef, Component, RefObject } from 'preact';
//import { signal, Signal } from '@preact/signals';
import { EditorState } from '@codemirror/state';
import { EditorView, basicSetup } from 'codemirror';
import * as pearchive from './pearchive';
import {Api} from './api';

import './style.css';

enum FileKind {
    Editor,
    Blob,
}

type FileId = string;
type ImageId = string;

type AppState = {
    images: Map<ImageId, Api.Image>,
    selectedImage?: ImageId,
    cmd?: string,
    lastStatus?: string,
}

class File {
    static _id = 0;

    id: string;
    path: string;
    kind: FileKind;
    editorState?: EditorState = null;
    data: string|ArrayBuffer;

    static _next_id(): number {
        File._id += 1;
        return File._id;
    }

    constructor(path, kind, data) {
        this.id = File._next_id().toString();
        this.path = path;
        this.kind = kind;
        this.data = data;
        if (this.kind === FileKind.Editor) {
            this.editorState = EditorState.create({
                doc: data,
            });
        }
    }

    static makeFile(path, data: string|ArrayBuffer): File {
        if (typeof data === 'string') {
            return new File(path, FileKind.Editor, data);
        } else {
            return new File(path, FileKind.Blob, data);
        }
    }

    displayName() {
        return this.path;
    }
};

const imageName = (info) => `${info.registry}/${info.repository}/${info.tag}`;

// TODO this just unconditionally overrides Entrypoint and Cmd with a string split of cmd
function computeFullCommand(image: Api.Image, userCmd: string): {entrypoint?: string[], cmd?: string[]} {
    // let env = image.config.config.Env ?? [];
    let parts = userCmd.split(/\s+/); // TODO handle quotes
    // todo handle env
    //if (parts[0] === '$entrypoint') {
    //    acc.extend(image.config.config.Entrypoint ?? []);
    //    parts = parts.slice(1);
    //}
    //if (parts[0] === '$cmd') {
    //    acc.extend(image.config.config.Cmd ?? []);
    //    parts = parts.slice(1);
    //}
    return {entrypoint: [], cmd: parts};
}

class FileStore {
    files: Map<FileId, File> = new Map();
    active?: string = null;

    constructor(files?: Map<FileId, File>, active?: FileId) {
        this.files = files ?? new Map();
        this.active = active ?? null;
    }

    static from(inputs: {path: string, data: string|ArrayBuffer}[]): FileStore {
        if (inputs.length === 0) return new FileStore();
        let files = new Map(inputs.map(({path,data}) => {
            let f = File.makeFile(path, data);
            return [f.id, f];
        }));
        let active = files.keys().next().value;
        return new FileStore(files, active);
    }

    addTextFile(path: string, data: string|ArrayBuffer): FileStore {
        let f = File.makeFile(path, data);
        let files = new Map(this.files);
        files.set(f.id, f);
        let active = this.active ?? f.id;
        return new FileStore(files, active);
    }

    addFiles(inputs: {path: string, data: string|ArrayBuffer}[]): FileStore {
        if (inputs.length === 0) return this;
        let fs = inputs.map(({path,data}) => File.makeFile(path, data));
        let files = new Map(this.files);
        for (let f of fs) {
            files.set(f.id, f);
        }
        let active = this.active ?? files.keys().next().value;
        return new FileStore(files, active);
    }

    getActive(): File | null {
        return this.files.get(this.active) ?? null;
    }

    setActive(file: File): FileStore {
        // do we have to copy this?
        this.active = file.id;
        return this;
    }
}

class Editor extends Component {
    ref = createRef();
    editor?: EditorView;
    props: {
        readOnly: boolean,
        placeholder?: string,
    };
    state: {
        store: FileStore,
    };

    constructor({readOnly, placeholder}: {readOnly?: boolean, placeholder?: string}) {
        super({placeholder,readOnly});
        this.state = {
            store: new FileStore(),
        };
    }

    componentDidMount() {
        this.editor = new EditorView({
          extensions: [
              basicSetup,
              ...(this.props.readOnly ? [EditorState.readOnly.of(true)] : []),
          ],
          parent: this.ref.current,
        });
    }

    commitActive() {
        let {store} = this.state;
        let active = store.getActive();
        if (active?.editorState === null) return;
        active.editorState = this.editor.state;
        active.data = active.editorState.doc.toString();
    }

    getFiles(): File[] {
        this.commitActive();
        return Array.from(this.state.store.files.values());
    }

    addFiles(files: {path: string, data: string|ArrayBuffer}[]) {
        let store = this.state.store.addFiles(files);
        this.setState({store: store});
    }

    setFiles(files: {path: string, data: string|ArrayBuffer}[]) {
        let store = FileStore.from(files);
        let active = this.state.store.getActive();
        if (active !== null) {
            console.log(active, store.files);
            for (let f of store.files.values()) {
                console.log('active check', f.path, active.path);
                if (f.path === active.path) {
                    console.log('setting active');
                    store.active = f.id;
                    break;
                }
            }
        }
        this.setState({store: store});
    }

    editFile(file: File) {
        this.commitActive();
        this.setState({store: this.state.store.setActive(file)});
    }

    // this.props, this.state
    render({placeholder,readOnly}, {store}) {
        let tabs = Array.from(store.files.values(), (file: File) => {
            let className = 'tab mono';
            if (store.active == file.id) {
                className += ' selected';
            }
            return (
                <button
                    className={className}
                    key={file.id} onClick={() => this.editFile(file)}>
                    {file.displayName()}
                </button>
            );
        });
        if (this.editor && store.active) {
            let f = store.files.get(store.active);
            if (f.kind == FileKind.Editor) {
                this.editor.setState(f.editorState);
            }
        }
        return (
            <div class="editorContainer">
                {tabs}
                <div class="cmContainer" ref={this.ref}></div>
            </div>
        );
    }
}

class App extends Component {
    r_inputEditor: RefObject<Editor> = createRef();
    r_outputEditor: RefObject<Editor> = createRef();
    state: AppState = {
        images: new Map(),
        selectedImage: null,
        cmd: null,
        lastStatus: null,
    };

    get inputEditor():  Editor { return this.r_inputEditor.current; }
    get outputEditor(): Editor { return this.r_outputEditor.current; }

    componentDidMount() {
        // if you execute these back to back they don't both get applied...
        this.inputEditor.addFiles([
            {path:'test.sh', data:'echo "hello world"\ncat /run/pe/input/f1/data.txt > /run/pe/output/data.txt\nls -ln /run/pe'},
            {path:'blob', data: new Uint8Array([0, 0, 0, 0, 0])},
            //{path:'data.txt', data:'hi this is some data'},
            {path:'f1/dataf1.txt', data:'hi this is some data'},
            {path:'f1/f2/dataf1f2.txt', data:'hi this is some data'},
            {path:'f2/dataf2.txt', data:'hi this is some data'},
        ]);
        this.setState({cmd: 'sh /run/pe/input/test.sh'});

        this.fetchImages();

        setTimeout(() => {
            let y = pearchive.packArchiveV1(this.inputEditor.getFiles());
            // only firefox has a Blob.bytes() method

            y.arrayBuffer().then(buf=>{
                let bytes = new Uint8Array(buf);
                console.log('----------------   packed -----------------------');
                console.log(bytes);
                console.log('---------------- unpacked (uint8array) -----------------------');
                console.log(pearchive.unpackArchiveV1(bytes));
                console.log('---------------- unpacked2 (arraybuffer) -----------------------');
                console.log(pearchive.unpackArchiveV1(buf));
                //console.log('---------------- unpacked2 (dataview) -----------------------');
                //console.log(pearchive.unpackArchiveV1(new DataView(buf, 62)));
            });
        }, 100);
    }

    async run(event) {
        event.preventDefault();

        let {images,selectedImage,cmd} = this.state;
        if (selectedImage === null) {
            console.warn('cant run without an image');
            return;
        }
        if (cmd === null) {
            console.warn('cant run without a cmd');
            return;
        }
        let image = images.get(selectedImage);

        let y = pearchive.packArchiveV1(this.inputEditor.getFiles());
        let z = pearchive.combineRequestAndArchive({
            'cmd': cmd.split(/\s+/),
        }, y);

        let req = new Request(window.location.origin + image.links.runi, {
            method: 'POST',
            body: z,
            headers: {
                'Content-type': 'application/x.pe.archivev1',
            }
        });
        const response = await fetch(req);
        if (!response.ok) {
            console.error(response);
            return;
        }
        const body = await response.arrayBuffer();
        let [responseJson, archiveSlice] = pearchive.splitResponseAndArchive(body);
        console.log(responseJson);
        let responseTyped: Api.Runi.Response = responseJson;
        let lastStatus = (() => {
            switch (responseTyped.kind) {
                case 'Ok': return {Ok: {siginfo: responseTyped.siginfo}};
                case 'Overtime': return {Overtime: {siginfo: responseTyped.siginfo}};
                case 'Panic': return {Panic: {message: responseTyped.message}};
            }
            return null;
        })();
        this.setState({lastStatus: lastStatus != null ? JSON.stringify(lastStatus) : null});
        let returnFiles = pearchive.unpackArchiveV1(archiveSlice);
        returnFiles.sort((a, b) => a.path.localeCompare(b.path));
        //console.log(returnFiles);
        //console.log(archiveSlice);
        this.outputEditor.setFiles(returnFiles);
    }

    async fetchImages() {
        let response = await fetch(window.location.origin + '/api/v1/images');
        if (response.ok) {
            let json = await response.json();
            if (json.images?.length > 0) {
                let images = new Map(json.images.map(image => [image.info.digest, image]));
                this.setState({
                    images: images,
                    selectedImage: images.keys().next().value,
                })
            }
        } else {
            console.error(response);
        }
    }

    onImageSelect(event) {
        this.setState({selectedImage: event.target.value});
    }

    onCmdChange(event) {
        this.setState({cmd: event.target.value});
    }

    // this.props, this.state
    render({}, {images,selectedImage,cmd,lastStatus}) {
        console.log('render', this.state);
        // firefox supports Map().values().map(), but chrome doesn't
        let imageOptions = Array.from(images.values(), ({info,links}) => {
            let name = imageName(info);
            return <option key={info.digest} value={links.runi}>{name}</option>;
        });

        let imageDetails = [];
        let fullCommand = '';
        if (selectedImage !== null) {
            let image = images.get(selectedImage);
            imageDetails = (
                <details>
                    <summary>About this image</summary>
                    <a href={image.links.upstream} rel="nofollow">{imageName(image.info)}</a>
                    <p>These are a subset of properties defined for this image. See <a href="https://github.com/opencontainers/image-spec/blob/main/config.md#properties" rel="nofollow">the OCI image spec</a> for more information.</p>
                    <dl>
                        <dt>Env</dt>
                        <dd class="mono">{JSON.stringify(image.config.config.Env ?? [])}</dd>
                        <dt>Entrypoint</dt>
                        <dd class="mono">{JSON.stringify(image.config.config.Entrypoint ?? [])}</dd>
                        <dt>Cmd</dt>
                        <dd class="mono">{JSON.stringify(image.config.config.Cmd ?? [])}</dd>
                    </dl>

                </details>
            );
            fullCommand = JSON.stringify(computeFullCommand(image, cmd).cmd);
        }
        return <div>
            <form>
                <select onChange={e => this.onImageSelect(e)}>
                    {imageOptions}
                </select>
                <input className="mono" type="text" value={cmd} onChange={e => this.onCmdChange(e)} placeholder="env $entrypoint $cmd < /dev/null" />
                <button className="mono" onClick={e => this.run(e)}>Run</button>
                <span className="mono">{fullCommand}</span>
                <span className="mono">{lastStatus ?? ''}</span>

                {imageDetails}


            </form>

            <div id="editorSideBySide">
                <Editor ref={this.r_inputEditor} />
                <Editor
                    ref={this.r_outputEditor}
                    readOnly={true}
                    placeholder={PLACEHOLDER_DIRECTIONS} />
            </div>
        </div>
    }
}

const PLACEHOLDER_DIRECTIONS = `Run something to see the output...`;
// TODO something about the args going in not being compat with the Component type signature
render(<App/>, document.getElementById('app'));

//const response = await fetch(window.location.origin + '/api/v1/images');
//console.log(response);
//if (response.ok) {
//    const json = await response.json();
//    s_images.value = json;
//    if (json.images?.length > 0) {
//        s_chosenImage.value = json.images[0].links.runi;
//    }
//} else {
//    console.error(response);
//}
//
//<Editor
//  height="80vh"
//  theme="vs-dark"
//  path={file.name}
//  defaultLanguage={file.language}
//  defaultValue={file.value}
///>
//function getOrCeateFile(path, text) {
//    let uri = monaco.Uri.file(path);
//    return monaco.editor.getModel(uri) || monaco.editor.createModel(text, undefined, uri);
//}

