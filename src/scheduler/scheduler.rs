// Copyright (c) 2017 Stefan Lankes, RWTH Aachen University
//
// MIT License
//
// Permission is hereby granted, free of charge, to any person obtaining
// a copy of this software and associated documentation files (the
// "Software"), to deal in the Software without restriction, including
// without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to
// permit persons to whom the Software is furnished to do so, subject to
// the following conditions:
//
// The above copyright notice and this permission notice shall be
// included in all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE
// LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION
// WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

use core::sync::atomic::{AtomicUsize, Ordering};
use core::ptr::Shared;
use scheduler::task::*;
use arch::irq::{irq_nested_enable,irq_nested_disable};
use arch::replace_boot_stack;
use logging::*;
use synch::spinlock::*;
use alloc::VecDeque;
use alloc::boxed::Box;
use alloc::btree_map::*;

static TID_COUNTER: AtomicUsize = AtomicUsize::new(0);

extern {
	pub fn switch(old_stack: *const usize, new_stack: usize);
}

pub struct Scheduler {
	/// task which is currently running
	current_task: Shared<Task>,
	/// idle task
	idle_task: Shared<Task>,
	/// queues of tasks, which are ready
	ready_queue: SpinlockIrqSave<PriorityTaskQueue>,
	/// queue of tasks, which are finished and can be released
	finished_tasks: SpinlockIrqSave<Option<VecDeque<TaskId>>>,
	/// map between task id and task control block
	tasks: SpinlockIrqSave<Option<BTreeMap<TaskId, Shared<Task>>>>,
	/// number of tasks managed by the scheduler
	no_tasks: AtomicUsize
}

impl Scheduler {
	/// Create a new scheduler
	pub const fn new() -> Scheduler {
		Scheduler {
			// I know that this is unsafe. But I know also that I initialize
			// the Scheduler (with add_idle_task correctly) before the system schedules task.
			current_task: unsafe { Shared::new_unchecked(0 as *mut Task) },
			idle_task: unsafe { Shared::new_unchecked(0 as *mut Task) },
			ready_queue: SpinlockIrqSave::new(PriorityTaskQueue::new()),
			finished_tasks: SpinlockIrqSave::new(None),
			tasks: SpinlockIrqSave::new(None),
			no_tasks: AtomicUsize::new(0)
		}
	}

	fn get_tid(&self) -> TaskId {
		loop {
			let id = TaskId::from(TID_COUNTER.fetch_add(1, Ordering::SeqCst));

			if self.tasks.lock().as_ref().unwrap().contains_key(&id) == false {
				return id;
			}
		}
	}

	/// add the current task as idle task the scheduler
	pub unsafe fn add_idle_task(&mut self) {
		// idle task is the first task for the scheduler => initialize queues and btree

		// initialize vector of queues
		*self.finished_tasks.lock() = Some(VecDeque::new());
		*self.tasks.lock() = Some(BTreeMap::new());
		let tid = self.get_tid();

		// boot task is implicitly task 0 and and the idle task of core 0
		let idle_box = Box::new(Task::new(tid, TaskStatus::TaskIdle, LOW_PRIO));
		let rsp = (*idle_box.stack).bottom();
		let ist = (*idle_box.ist).bottom();
		let idle_shared = Shared::new_unchecked(Box::into_raw(idle_box));

		self.idle_task = idle_shared;
		self.current_task = self.idle_task;

		// replace temporary boot stack by the kernel stack of the boot task
		replace_boot_stack(rsp, ist);

		self.tasks.lock().as_mut().unwrap().insert(tid, idle_shared);
	}

	/// Spawn a new task
	pub unsafe fn spawn(&mut self, func: extern fn(), prio: Priority) -> TaskId {
		let tid: TaskId;

		// do we have finished a task? => reuse it
		match self.finished_tasks.lock().as_mut().unwrap().pop_front() {
			None => {
				debug!("create new task control block");
				tid = self.get_tid();
				let mut task = Box::new(Task::new(tid, TaskStatus::TaskReady, prio));

				task.create_stack_frame(func);

				let shared_task = &mut Shared::new_unchecked(Box::into_raw(task));
				self.ready_queue.lock().push(prio, shared_task);
				self.tasks.lock().as_mut().unwrap().insert(tid, *shared_task);
			},
			Some(id) => {
				debug!("resuse existing task control block");

				tid = id;
				match self.tasks.lock().as_mut().unwrap().get_mut(&tid) {
					Some(task) => {
						// reset old task and setup stack frame
						task.as_mut().status = TaskStatus::TaskReady;
						task.as_mut().prio = prio;
						task.as_mut().last_stack_pointer = 0;

						task.as_mut().create_stack_frame(func);

						self.ready_queue.lock().push(prio, task);
					},
					None => panic!("didn't find task")
				}
			}
		}

		info!("create task with id {}", tid);

		// update the number of tasks
		self.no_tasks.fetch_add(1, Ordering::SeqCst);

		tid
	}

	/// Terminate the current task
	pub unsafe fn exit(&mut self) -> ! {
		if self.current_task.as_ref().status != TaskStatus::TaskIdle {
			info!("finish task with id {}", self.current_task.as_ref().id);
			self.current_task.as_mut().status = TaskStatus::TaskFinished;
			// update the number of tasks
			self.no_tasks.fetch_sub(1, Ordering::SeqCst);
		} else {
			panic!("unable to terminate idle task");
		}

		self.reschedule();

		// we should never reach this point
		panic!("exit failed!")
	}

	pub unsafe fn abort(&mut self) -> ! {
			if self.current_task.as_ref().status != TaskStatus::TaskIdle {
				info!("abort task with id {}", self.current_task.as_ref().id);
				self.current_task.as_mut().status = TaskStatus::TaskFinished;
				// update the number of tasks
				self.no_tasks.fetch_sub(1, Ordering::SeqCst);
			} else {
				panic!("unable to terminate idle task");
			}

			self.reschedule();

			// we should never reach this point
			panic!("abort failed!");
	}

	pub fn number_of_tasks(&self) -> usize {
		self.no_tasks.load(Ordering::SeqCst)
	}

	/// Block the current task
	pub unsafe fn block_current_task(&mut self) -> Shared<Task> {
		if self.current_task.as_ref().status == TaskStatus::TaskRunning {
			debug!("block task {}", self.current_task.as_ref().id);

			self.current_task.as_mut().status = TaskStatus::TaskBlocked;
			return self.current_task;
		} else {
			panic!("unable to block task {}", self.current_task.as_ref().id);
		}
	}

	/// Wakeup a blocked task
	pub unsafe fn wakeup_task(&mut self, mut task: Shared<Task>) {
		if task.as_ref().status == TaskStatus::TaskBlocked {
			let prio = task.as_ref().prio;

			debug!("wakeup task {}", task.as_ref().id);

			task.as_mut().status = TaskStatus::TaskReady;
			self.ready_queue.lock().push(prio, &mut Shared::new_unchecked(task.as_mut()));
		}
	}

	/// Determines the id of the current task
	#[inline(always)]
	pub fn get_current_taskid(&self) -> TaskId {
		unsafe { self.current_task.as_ref().id }
	}

	/// Determines the start addresses of the stacks
	#[inline(always)]
	pub fn get_current_stacks(&self) -> (usize, usize) {
		unsafe {
			((*self.current_task.as_ref().stack).bottom(), (*self.current_task.as_ref().ist).bottom())
		}
	}

	/// Determines the start address of kernel stack (rsp0)
	#[inline(always)]
	pub fn get_kernel_stack(&self) -> usize {
		unsafe {
			(*self.current_task.as_ref().stack).bottom()
		}
	}

	/// Determines the priority of the current task
	#[inline(always)]
	pub fn get_current_priority(&self) -> Priority {
		unsafe { self.current_task.as_ref().prio }
	}

	/// Determines the priority of the task with the 'tid'
	pub fn get_priority(&self, tid: TaskId) -> Priority {
		let mut prio: Priority = NORMAL_PRIO;

		match self.tasks.lock().as_ref().unwrap().get(&tid) {
			Some(task) => prio = unsafe { task.as_ref().prio },
			None => { info!("didn't find current task"); }
		}

		prio
	}

	unsafe fn get_next_task(&mut self) -> Option<Shared<Task>> {
		let mut prio = LOW_PRIO;
		let status: TaskStatus;

		// if the current task is runable, check only if a task with
		// higher priority is available
		if self.current_task.as_ref().status == TaskStatus::TaskRunning {
			prio = self.current_task.as_ref().prio;
		}
		status = self.current_task.as_ref().status;

		match self.ready_queue.lock().pop_with_prio(prio) {
			Some(mut task) => {
				task.as_mut().status = TaskStatus::TaskRunning;
				return Some(task)
			},
			None => {}
		}

		if status != TaskStatus::TaskRunning && status != TaskStatus::TaskIdle {
			// current task isn't able to run and no other task available
			// => switch to the idle task
			Some(self.idle_task)
		} else {
			None
		}
	}

	pub unsafe fn schedule(&mut self) {
		// do we have a task, which is ready?
		match self.get_next_task() {
			Some(next_task) => {
				let old_id: TaskId = self.current_task.as_ref().id;

				if self.current_task.as_ref().status == TaskStatus::TaskRunning {
					self.current_task.as_mut().status = TaskStatus::TaskReady;
					self.ready_queue.lock().push(self.current_task.as_ref().prio,
						&mut self.current_task);
				} else if self.current_task.as_ref().status == TaskStatus::TaskFinished {
					self.current_task.as_mut().status = TaskStatus::TaskInvalid;
					// release the task later, because the stack is required
					// to call the function "switch"
					// => push id to a queue and release the task later
					self.finished_tasks.lock().as_mut().unwrap().push_back(old_id);
				}

				let next_stack_pointer = next_task.as_ref().last_stack_pointer;
				let old_stack_pointer = &self.current_task.as_ref().last_stack_pointer as *const usize;

				self.current_task = next_task;

				debug!("switch task from {} to {}", old_id, next_task.as_ref().id);

				switch(old_stack_pointer, next_stack_pointer);
			},
			None => {}
		}
	}

	/// Check if a finisched task could be deleted.
	unsafe fn cleanup_tasks(&mut self)
	{
		// do we have finished tasks? => drop first tasks => deallocate implicitly the stack
		match self.finished_tasks.lock().as_mut().unwrap().pop_front() {
			Some(id) => {
				match self.tasks.lock().as_mut().unwrap().remove(&id) {
					Some(task) => drop(Box::from_raw(task.as_ptr())),
					None => info!("unable to drop task {}", id)
				}
			},
			None => {}
	 	}
	}

	/// Triggers the scheduler to reschedule the tasks
	#[inline(always)]
	pub unsafe fn reschedule(&mut self) {
		// someone want to give up the CPU
		// => we have time to cleanup the system
		self.cleanup_tasks();

		let flags = irq_nested_disable();
		self.schedule();
		irq_nested_enable(flags);
	}
}
