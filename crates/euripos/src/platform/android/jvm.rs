//! The JVM side: the `JavaVM` and application `Context` the app gave
//! `ndk-context`, the monitor's Java loaded from the embedded dex, and the
//! platform verifier's initialisation.

use std::ffi::c_void;
use std::panic;
use std::sync::OnceLock;

use jni::errors::{Error, LogErrorAndDefault};
use jni::objects::{Global, JClass, JObject, JString};
use jni::sys::{jint, jlong};
use jni::{Env, EnvUnowned, JValue, JavaVM, NativeMethod, jni_sig, jni_str};

use super::{Report, proxy};

/// The monitor's Java, compiled by `mise run android:dex` from
/// `NetworkMonitor.java`.
const DEX: &[u8] = include_bytes!("monitor.dex");

/// The JVM and the application `Context`, held for the process's life.
pub(super) struct Jvm {
    vm: JavaVM,
    context: Global<JObject<'static>>,
}

/// The JVM the app gave `ndk-context`, or `None` where it gave none.
///
/// `ndk-context` panics when asked before the app initialised it, and has no
/// way to ask without. The panic is caught, so an app that never initialises
/// it gets no network facts and a TLS error rather than a crash; built with
/// `panic = "abort"`, it aborts instead (docs/design/android.md,
/// "Embedding").
pub(super) fn jvm() -> Option<&'static Jvm> {
    static JVM: OnceLock<Option<Jvm>> = OnceLock::new();
    JVM.get_or_init(|| {
        let android = panic::catch_unwind(ndk_context::android_context).ok()?;
        if android.vm().is_null() || android.context().is_null() {
            return None;
        }
        // SAFETY: `ndk-context` holds the process's `JavaVM`, as its
        // initialiser promised.
        let vm = unsafe { JavaVM::from_raw(android.vm().cast()) };
        let context = vm
            .attach_current_thread(|env| -> Result<_, Error> {
                // SAFETY: `ndk-context` holds a global reference to the
                // application `Context`, valid for the process's life.
                let context = unsafe { JObject::from_raw(env, android.context().cast()) };
                env.new_global_ref(context)
            })
            .ok()?;
        Some(Jvm { vm, context })
    })
    .as_ref()
}

impl Jvm {
    /// Initialise rustls-platform-verifier with this JVM's `Context`.
    pub(super) fn init_verifier(&self) -> Result<(), Error> {
        self.vm.attach_current_thread(|env| {
            // SAFETY: the global reference outlives the call.
            let context = unsafe { JObject::from_raw(env, self.context.as_raw()) };
            rustls_platform_verifier::android::init_with_env(env, context)
        })
    }

    /// Load the monitor's Java, bind its native method to [`changed`] and
    /// start it: from then on every change to the default network is
    /// reported to [`super::report`].
    pub(super) fn start_monitor(&self) -> Result<(), Error> {
        self.vm.attach_current_thread(|env| {
            let context = &self.context;
            let parent = env
                .call_method(
                    context,
                    jni_str!("getClassLoader"),
                    jni_sig!("()Ljava/lang/ClassLoader;"),
                    &[],
                )?
                .l()?;
            let bytes = env.byte_array_from_slice(DEX)?;
            let buffer = env
                .call_static_method(
                    jni_str!("java/nio/ByteBuffer"),
                    jni_str!("wrap"),
                    jni_sig!("([B)Ljava/nio/ByteBuffer;"),
                    &[JValue::Object(&bytes)],
                )?
                .l()?;
            let loader = env.new_object(
                jni_str!("dalvik/system/InMemoryDexClassLoader"),
                jni_sig!("(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V"),
                &[JValue::Object(&buffer), JValue::Object(&parent)],
            )?;
            let name = env.new_string("org.getkono.euripos.NetworkMonitor")?;
            let class = env
                .call_method(
                    &loader,
                    jni_str!("loadClass"),
                    jni_sig!("(Ljava/lang/String;)Ljava/lang/Class;"),
                    &[JValue::Object(&name)],
                )?
                .l()?;
            let class = env.cast_local::<JClass>(class)?;
            let changed = changed as extern "system" fn(_, _, _, _, _, _, _, _, _);
            // SAFETY: `changed` takes the arguments the signature names, in
            // order, after the env and the class of a static method.
            unsafe {
                env.register_native_methods(
                    &class,
                    &[NativeMethod::from_raw_parts(
                        jni_str!("changed"),
                        jni_str!("(JIILjava/lang/String;ILjava/lang/String;Ljava/lang/String;)V"),
                        changed as *mut c_void,
                    )],
                )?;
            }
            env.call_static_method(
                &class,
                jni_str!("start"),
                jni_sig!("(Landroid/content/Context;)V"),
                &[JValue::Object(context)],
            )?;
            Ok(())
        })
    }
}

/// `NetworkMonitor.changed`: the default network as the Java side sees it
/// now. See `NetworkMonitor.java` for the arguments.
#[allow(
    clippy::too_many_arguments,
    reason = "the Java method's signature, one argument per fact"
)]
extern "system" fn changed<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    transports: jint,
    flags: jint,
    host: JString<'local>,
    port: jint,
    exclusions: JString<'local>,
    pac: JString<'local>,
) {
    env.with_env(|env| -> Result<(), Error> {
        let exclusions = string(env, &exclusions)?;
        super::report(Report {
            handle: handle.cast_unsigned(),
            transports: transports.cast_unsigned(),
            flags: flags.cast_unsigned(),
            proxy: proxy::Settings::new(
                string(env, &host)?,
                port,
                exclusions.as_deref(),
                string(env, &pac)?,
            ),
            // Counted from the reports in turn, by `super::report`.
            resumes: 0,
        });
        Ok(())
    })
    .resolve::<LogErrorAndDefault>();
}

/// A Java string, `None` where it is `null`.
fn string(env: &Env<'_>, string: &JString<'_>) -> Result<Option<String>, Error> {
    if string.is_null() {
        return Ok(None);
    }
    string.try_to_string(env).map(Some)
}
