//! CMIS subsystem — port of the Python `cmis/` subpackage.
//! `cmis_manager_task` <- cmis/cmis_manager_task.py (the CMIS datapath SM).

pub mod cmis_manager_task;

pub use cmis_manager_task::CmisManagerTask;
